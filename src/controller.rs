//! Shared application state behind the GUI, the tray menu, the scheduler and the MCP bridge:
//! configuration, DPAPI secrets, backup jobs, and the named operations agents call.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use chrono::{DateTime, Local, Timelike, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::config::{
    AppConfig, BASIC_EXCLUDE_IF_PRESENT, BASIC_EXCLUDES, BASIC_UNITY_PROJECT_EXCLUDES,
    BackupTarget, MAX_INTERVAL_MINUTES, MAX_KEEP_DAILY, MAX_KEEP_LAST, MAX_KEEP_MONTHLY,
    MAX_KEEP_WEEKLY, MIN_INTERVAL_MINUTES, MaintenanceRole, ScheduleConfig, WriterState,
    normalize_daily_at, normalize_maintenance_at, parse_daily_at, validate_project_key,
    validate_user_tags,
};
use crate::db::HistoryDb;
use crate::error::{AppError, AppResult};
use crate::fs_safety::{
    canonicalize_existing_dir, find_unity_lockfile, is_git_repository, paths_overlap,
    snapshot_path_folder_name, validate_restore_destination, windows_path_to_restic_subpath,
};
use crate::notify;
use crate::r2_s3::{self, R2DeleteProgress, R2DeleteReport, R2DeleteScope};
use crate::restic::excludes::{exclude_fingerprint, restore_delete_exclude_patterns};
use crate::restic::runner::{
    self, BackupTargetOptions, PromoteHandoffOutcome, RETRY_LOCK_MAINTENANCE,
    RETRY_LOCK_MANUAL_OR_HANDOFF, RETRY_LOCK_READ, RETRY_LOCK_SCHEDULED_BACKUP,
    RestoreSubpathOptions,
};
use crate::restic::sidecar::{ResticBinaryInfo, probe_restic_binary};
use crate::restic::types::{
    BackupProgressState, FolderBytes, ResticBackupSummary, TAG_HANDOFF_CLAIMED,
    TAG_HANDOFF_FROM_PREFIX, TAG_HANDOFF_RELEASED, TAG_HANDOFF_SAFETY_BACKUP, TAG_HANDOFF_STAGING,
    TAG_PROJECT_PREFIX,
};
use crate::scheduler::{
    backup_is_overdue, first_run_at, last_success_at, maintenance_is_due, next_run_after,
    verify_is_due,
};
use crate::secrets::{SecretsReadiness, StoredSecrets, load_secrets, save_secrets};
use crate::sync::{self, ProjectSyncReport, ProjectSyncSummary, TakeoverParams, TakeoverReport};

pub(crate) fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Stable fingerprint of the configured Cloudflare R2 destination so long-running operations
/// (backup / release / takeover) detect if storage settings were switched mid-flight.
fn storage_destination_key(cfg: &AppConfig) -> String {
    format!(
        "r2|{}|{}|{}|{}|{}",
        cfg.r2.account_id.trim(),
        cfg.r2.endpoint_override.as_deref().unwrap_or("").trim(),
        cfg.r2.normalized_bucket(),
        cfg.r2.normalized_prefix(),
        cfg.repository_url().unwrap_or_default(),
    )
}

/// Attempts to delete a partial/aborted staging snapshot and returns a formatted note if cleanup
/// itself failed so errors are never silently dropped.
fn cleanup_staging_snapshot_note(
    cfg: &AppConfig,
    secrets: &StoredSecrets,
    snapshot_id: &str,
) -> String {
    match runner::forget_snapshot(cfg, secrets, snapshot_id) {
        Ok(_) => String::new(),
        Err(err) => {
            format!("（さらに仮スナップショット {snapshot_id} の自動削除にも失敗しました: {err}）")
        }
    }
}

/// Result of checking whether another PC concurrently claimed/updated a shared project.
enum ConcurrentClaimCheck {
    Clear,
    ClaimedByOther(String),
}

/// Checks an in-memory snapshot list for `project:<key>` for any effective snapshot published by
/// ANOTHER PC that either:
/// 1. Was NOT present at Stage 1 (`initial_project_snap_ids`), OR
/// 2. Carries `handoff:from:<source_snap_id>` (meaning another PC already claimed `source_snap_id`,
///    even if its system clock was skewed behind `source_snap.time` before Stage 1).
fn find_concurrent_remote_claim_in_snapshots(
    snaps: &[crate::restic::types::ResticSnapshot],
    project_key: &str,
    local_machine: &str,
    source_snap_id: &str,
    initial_project_snap_ids: &std::collections::BTreeSet<String>,
    my_promoted_id: Option<&str>,
) -> ConcurrentClaimCheck {
    let mut others: Vec<&crate::restic::types::ResticSnapshot> = snaps
        .iter()
        .filter(|s| {
            s.project_key() == Some(project_key)
                && !s.is_safety_backup()
                && !s.is_handoff_staging()
                && Some(s.id.as_str()) != my_promoted_id
                && !s.normalized_host().eq_ignore_ascii_case(local_machine)
                && (!initial_project_snap_ids.contains(&s.id)
                    || s.handoff_from_snapshot_id() == Some(source_snap_id))
        })
        .collect();
    others.sort_by(|a, b| sync::compare_snapshot_time(b, a));
    if let Some(other) = others.first() {
        let host_now = other.normalized_host();
        return ConcurrentClaimCheck::ClaimedByOther(format!(
            "復元・公開中に他 PC（{host_now}）がプロジェクト \"{project_key}\" の引き継ぎスナップショット（{}）を公開済みのため、二重作業を防ぐため作業権の取得を中止しました。最新状態を確認してからやり直してください",
            other.display_short_id()
        ));
    }
    ConcurrentClaimCheck::Clear
}

/// Verifies against a fresh remote snapshot listing for `project:<key>` that no other PC has
/// claimed `source_snap_id` or published a new effective snapshot since Stage 1.
/// Transient network/lock errors return `Err` without falsely classifying as a confirmed
/// concurrent claim.
fn verify_no_concurrent_remote_claim(
    cfg: &AppConfig,
    secrets: &StoredSecrets,
    project_key: &str,
    local_machine: &str,
    source_snap_id: &str,
    initial_project_snap_ids: &std::collections::BTreeSet<String>,
    cancel_flag: &Arc<AtomicBool>,
) -> AppResult<ConcurrentClaimCheck> {
    let tag_filter = format!("{TAG_PROJECT_PREFIX}{project_key}");
    let fresh_snaps = runner::list_snapshots_filtered_cancellable(
        cfg,
        secrets,
        None,
        Some(&tag_filter),
        None,
        Some(RETRY_LOCK_MANUAL_OR_HANDOFF),
        cancel_flag,
    )?;
    Ok(find_concurrent_remote_claim_in_snapshots(
        &fresh_snaps,
        project_key,
        local_machine,
        source_snap_id,
        initial_project_snap_ids,
        None,
    ))
}

fn summarize_forget_audit(raw_json: &str, dry_run: bool, scheduled: bool) -> String {
    let suffix = if scheduled { " (maintenance slot)" } else { "" };
    if let Ok(val) = serde_json::from_str::<Value>(raw_json) {
        let keep = val.get("totalKeep").and_then(Value::as_u64).unwrap_or(0);
        let remove = val.get("totalRemove").and_then(Value::as_u64).unwrap_or(0);
        let safety = val
            .get("protectedSafetyBackups")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        format!("dry_run={dry_run} keep={keep} remove={remove} safety_protected={safety}{suffix}")
    } else {
        format!("dry_run={dry_run}{suffix}")
    }
}

/// A string argument of an agent operation that must be present.
fn required_str<'a>(args: &'a Value, key: &str) -> AppResult<&'a str> {
    args.get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| AppError::InvalidArgument(format!("{key} is required")))
}

/// Optional string argument that can be omitted (`Ok(None)`) or a string (`Ok(Some(trimmed))`).
fn optional_trimmed_str(obj: &Value, key: &str) -> AppResult<Option<String>> {
    match obj.get(key) {
        None => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.trim().to_owned())),
        Some(_) => Err(AppError::InvalidArgument(format!("{key} must be a string"))),
    }
}

/// Optional nullable string argument: omitted -> `Ok(None)`, `null` or blank -> `Ok(Some(None))`,
/// non-blank string -> `Ok(Some(Some(trimmed)))`.
fn optional_nullable_str(obj: &Value, key: &str) -> AppResult<Option<Option<String>>> {
    match obj.get(key) {
        None => Ok(None),
        Some(Value::Null) => Ok(Some(None)),
        Some(Value::String(s)) => {
            let trimmed = s.trim();
            Ok(Some((!trimmed.is_empty()).then(|| trimmed.to_owned())))
        }
        Some(_) => Err(AppError::InvalidArgument(format!(
            "{key} must be a string or null"
        ))),
    }
}

/// Optional integer argument clamped to `min..=max`.
fn optional_u32_clamped(obj: &Value, key: &str, min: u32, max: u32) -> AppResult<Option<u32>> {
    match obj.get(key) {
        None => Ok(None),
        Some(v) => {
            let n = v.as_u64().ok_or_else(|| {
                AppError::InvalidArgument(format!("{key} must be a positive integer"))
            })?;
            Ok(Some((n as u32).clamp(min, max)))
        }
    }
}

fn reject_unknown_keys(obj: &serde_json::Map<String, Value>, allowed: &[&str]) -> AppResult<()> {
    for key in obj.keys() {
        if !allowed.contains(&key.as_str()) {
            return Err(AppError::InvalidArgument(format!(
                "unknown field in arguments: {key}"
            )));
        }
    }
    Ok(())
}

/// Optional boolean argument: omitted -> `Ok(None)`, bool -> `Ok(Some(b))`, other -> `Err`.
fn optional_bool(obj: &Value, key: &str) -> AppResult<Option<bool>> {
    match obj.get(key) {
        None => Ok(None),
        Some(Value::Bool(b)) => Ok(Some(*b)),
        Some(_) => Err(AppError::InvalidArgument(format!(
            "{key} must be a boolean"
        ))),
    }
}

/// Optional integer argument validated within `min..=max`.
fn optional_u64_in_range(obj: &Value, key: &str, min: u64, max: u64) -> AppResult<Option<u64>> {
    match obj.get(key) {
        None => Ok(None),
        Some(v) => {
            let n = v.as_u64().ok_or_else(|| {
                AppError::InvalidArgument(format!(
                    "{key} must be an integer between {min} and {max}"
                ))
            })?;
            if !(min..=max).contains(&n) {
                return Err(AppError::InvalidArgument(format!(
                    "{key} must be between {min} and {max} (got {n})"
                )));
            }
            Ok(Some(n))
        }
    }
}

/// Optional u32 argument validated within `min..=max` without truncating casts.
fn optional_u32_in_range(obj: &Value, key: &str, min: u32, max: u32) -> AppResult<Option<u32>> {
    let Some(n) = optional_u64_in_range(obj, key, u64::from(min), u64::from(max))? else {
        return Ok(None);
    };
    let value = u32::try_from(n).map_err(|_| {
        AppError::InvalidArgument(format!("{key} must be between {min} and {max} (got {n})"))
    })?;
    Ok(Some(value))
}

/// Optional string-array argument that strictly rejects non-arrays, non-string elements, and
/// (when requested) empty arrays or blank elements so malformed filters never silently broaden
/// an operation.
fn strict_str_list(
    args: &Value,
    key: &str,
    allow_empty: bool,
    reject_blank_items: bool,
) -> AppResult<Option<Vec<String>>> {
    match args.get(key) {
        None => Ok(None),
        Some(Value::Array(items)) => {
            if !allow_empty && items.is_empty() {
                return Err(AppError::InvalidArgument(format!(
                    "{key} must not be empty when specified"
                )));
            }
            let mut out = Vec::with_capacity(items.len());
            for item in items {
                let s = item.as_str().ok_or_else(|| {
                    AppError::InvalidArgument(format!("{key} must be an array of strings"))
                })?;
                if reject_blank_items && s.trim().is_empty() {
                    return Err(AppError::InvalidArgument(format!(
                        "{key} must not contain empty or blank entries"
                    )));
                }
                out.push(s.to_owned());
            }
            Ok(Some(out))
        }
        Some(_) => Err(AppError::InvalidArgument(format!(
            "{key} must be an array of strings"
        ))),
    }
}

fn basic_list(enabled: bool, list: &[&str]) -> Vec<String> {
    if enabled {
        list.iter().map(|s| (*s).to_owned()).collect()
    } else {
        Vec::new()
    }
}

/// Result of one scheduled backup attempt, used by the scheduler for retry & notification policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ScheduledBackupOutcome {
    Success,
    Deferred,
    Failed,
    SkippedNoActiveTargets,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AppStatusReport {
    pub version: String,
    pub config_path: String,
    pub machine_name: String,
    pub maintenance_role: MaintenanceRole,
    pub storage_backend: String,
    pub storage_configured: bool,
    pub repository_url: Option<String>,
    pub r2_configured: bool,
    pub secrets_readiness: SecretsReadiness,
    /// Non-secret diagnostic when `credentials.dat` exists on disk but cannot be decrypted/parsed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secrets_error: Option<String>,
    pub restic_binary: ResticBinaryInfo,
    pub schedule_enabled: bool,
    pub interval_minutes: u64,
    /// "HH:MM" times when the schedule runs at fixed times each day; `interval_minutes` is unused then.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub daily_at: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub maintenance_at: Option<String>,
    pub next_scheduled_at: Option<String>,
    pub last_status_message: String,
    pub active_progress: BackupProgressState,
    /// In-flight Cloudflare R2 prefix/bucket scan or deletion progress, when active.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_r2_delete: Option<R2DeleteProgress>,
    pub enabled_target_count: usize,
    pub active_target_count: usize,
    pub total_target_count: usize,
    /// Cached summary of shared project sync states from local SQLite.
    #[serde(default)]
    pub project_sync_summary: ProjectSyncSummary,
    /// Consecutive newest real backup runs deferred by repository lock contention.
    #[serde(default)]
    pub consecutive_deferred_count: usize,
    /// Other hostnames previously observed in the shared repository.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub observed_remote_hosts: Vec<String>,
    /// Built-in excludes in effect (`useBasicExcludes`); `config.json` does not list them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub basic_excludes: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub basic_exclude_if_present: Vec<String>,
    /// Folders excluded only directly under a Unity project, not wherever the name appears.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub basic_unity_project_excludes: Vec<String>,
    /// The latest restore check (`restic check --read-data-subset`), scheduled or manual.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_verify: Option<VerifyStatus>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VerifyStatus {
    pub at: String,
    pub ok: bool,
    pub detail: String,
}

impl AppStatusReport {
    /// Maximum minutes between consecutive scheduled runs (one day for a single daily time).
    pub fn schedule_period_minutes(&self) -> u64 {
        let mut mins: Vec<u64> = self
            .daily_at
            .iter()
            .filter_map(|s| parse_daily_at(s))
            .map(|t| u64::from(t.num_seconds_from_midnight() / 60))
            .collect();
        mins.sort_unstable();
        mins.dedup();
        match mins.len() {
            0 => self.interval_minutes,
            1 => 1_440,
            _ => {
                let mut max_gap = 1_440 - mins.last().unwrap() + mins[0];
                for pair in mins.windows(2) {
                    max_gap = max_gap.max(pair[1] - pair[0]);
                }
                max_gap
            }
        }
    }
}

pub struct SharedController {
    pub base_dir: PathBuf,
    pub config_path: PathBuf,
    pub db_path: PathBuf,
    config: Mutex<AppConfig>,
    pub progress: Arc<Mutex<BackupProgressState>>,
    pub r2_delete_progress: Arc<Mutex<R2DeleteProgress>>,
    pub cancel_flag: Arc<AtomicBool>,
    job_mutex: Mutex<()>,
    last_status: Mutex<String>,
    pub(crate) next_run_at: Mutex<Option<DateTime<Utc>>>,
    show_window_requested: AtomicBool,
    quit_requested: AtomicBool,
    /// A notification about overdue backups was shown and none has succeeded since.
    overdue_notified: AtomicBool,
    /// A notification about 3+ consecutive deferred backups was shown and none has succeeded since.
    deferred_notified: AtomicBool,
    restic_probe: Mutex<Option<(Option<String>, Instant, ResticBinaryInfo)>>,
}

impl SharedController {
    pub fn new(base_dir: PathBuf, config_path: PathBuf) -> AppResult<Arc<Self>> {
        std::fs::create_dir_all(&base_dir)?;
        let cfg = AppConfig::load_or_default(&config_path)?;
        // Kept as `r2_restic_history.db` across the rename to "Restic Tray" to preserve
        // existing backup runs and audit logs on upgrade.
        let db_path = base_dir.join("r2_restic_history.db");
        let db = HistoryDb::open(&db_path)?;
        let next_dt = cfg
            .schedule
            .enabled
            .then(|| first_run_at(&cfg.schedule, &Local::now(), last_success_at(&db)));
        let initial_status = "待機中 (Cloudflare R2 + Restic Ready)".to_owned();

        Ok(Arc::new(Self {
            base_dir,
            config_path,
            db_path,
            config: Mutex::new(cfg),
            progress: Arc::new(Mutex::new(BackupProgressState::default())),
            r2_delete_progress: Arc::new(Mutex::new(R2DeleteProgress::default())),
            cancel_flag: Arc::new(AtomicBool::new(false)),
            job_mutex: Mutex::new(()),
            last_status: Mutex::new(initial_status),
            next_run_at: Mutex::new(next_dt),
            show_window_requested: AtomicBool::new(false),
            quit_requested: AtomicBool::new(false),
            overdue_notified: AtomicBool::new(false),
            deferred_notified: AtomicBool::new(false),
            restic_probe: Mutex::new(None),
        }))
    }

    pub fn get_config(&self) -> AppConfig {
        lock(&self.config).clone()
    }

    pub fn update_config<F>(&self, mutator: F) -> AppResult<AppConfig>
    where
        F: FnOnce(&mut AppConfig) -> AppResult<()>,
    {
        let mut guard = lock(&self.config);
        let _file_lock = crate::config::lock_config_exclusive(&self.config_path)?;
        let mut candidate = if self.config_path.exists() {
            AppConfig::load_or_default_unlocked(&self.config_path)?
        } else {
            guard.clone()
        };
        mutator(&mut candidate)?;
        candidate.validate_and_normalize(&self.config_path)?;
        candidate.save_atomic(&self.config_path)?;
        let before = guard.schedule.clone();
        *guard = candidate.clone();
        self.reschedule_if_changed(&before, &guard.schedule);
        Ok(candidate)
    }

    pub fn reload_config_from_disk(&self) -> AppResult<AppConfig> {
        let mut guard = lock(&self.config);
        let _file_lock = crate::config::lock_config_shared(&self.config_path)?;
        let loaded = AppConfig::load_or_default_unlocked(&self.config_path)?;
        self.reschedule_if_changed(&guard.schedule, &loaded.schedule);
        *guard = loaded.clone();
        Ok(loaded)
    }

    /// Other config edits (targets, R2) keep the pending run, including a catch-up run.
    fn reschedule_if_changed(&self, before: &ScheduleConfig, after: &ScheduleConfig) {
        if before != after {
            *lock(&self.next_run_at) = after.enabled.then(|| next_run_after(after, &Local::now()));
        }
    }

    pub fn load_secrets(&self) -> AppResult<StoredSecrets> {
        Ok(load_secrets(&self.base_dir)?.unwrap_or_default())
    }

    pub fn save_secrets(&self, secrets: &StoredSecrets) -> AppResult<()> {
        save_secrets(&self.base_dir, secrets)?;
        self.record_audit("gui", "save_secrets", "Updated DPAPI credentials");
        Ok(())
    }

    /// Merges newly entered credential fields into the existing DPAPI vault and saves it.
    ///
    /// Returns an error without modifying `credentials.dat` if an existing vault file cannot be
    /// decrypted or parsed (e.g. copied from another Windows user or corrupted).
    pub fn update_secrets_partial(
        &self,
        access_key_id: Option<&str>,
        secret_access_key: Option<&str>,
        restic_password: Option<&str>,
    ) -> AppResult<()> {
        let mut stored = load_secrets(&self.base_dir)?.unwrap_or_default();
        if let Some(v) = access_key_id.map(str::trim).filter(|s| !s.is_empty()) {
            stored.r2_access_key_id = Some(v.to_owned());
        }
        if let Some(v) = secret_access_key.map(str::trim).filter(|s| !s.is_empty()) {
            stored.r2_secret_access_key = Some(v.to_owned());
        }
        if let Some(v) = restic_password.filter(|s| !s.trim().is_empty()) {
            stored.restic_password = Some(v.to_owned());
        }
        self.save_secrets(&stored)
    }

    pub fn set_status(&self, msg: impl Into<String>) {
        *lock(&self.last_status) = msg.into();
    }

    pub fn status_message(&self) -> String {
        lock(&self.last_status).clone()
    }

    pub fn request_show_window(&self) {
        self.show_window_requested.store(true, Ordering::Relaxed);
    }

    pub fn take_show_window_request(&self) -> bool {
        self.show_window_requested.swap(false, Ordering::Relaxed)
    }

    pub fn request_quit(&self) {
        self.quit_requested.store(true, Ordering::Relaxed);
    }

    pub fn is_quit_requested(&self) -> bool {
        self.quit_requested.load(Ordering::Relaxed)
    }

    pub fn record_audit(&self, actor: &str, op: &str, detail: &str) {
        if let Ok(db) = HistoryDb::open(&self.db_path) {
            let _ = db.record_audit(actor, op, detail);
        }
    }

    /// `restic version` spawns a process and the GUI polls status every few seconds even while
    /// hidden in the tray, so reuse a recent probe for the same configured path.
    fn cached_binary_probe(
        cache: &Mutex<Option<(Option<String>, Instant, ResticBinaryInfo)>>,
        override_path: Option<&str>,
        probe_fn: fn(Option<&str>) -> ResticBinaryInfo,
    ) -> ResticBinaryInfo {
        let mut guard = lock(cache);
        if let Some((key, at, info)) = guard.as_ref() {
            let ttl = Duration::from_secs(if info.available { 60 } else { 5 });
            if key.as_deref() == override_path && at.elapsed() < ttl {
                return info.clone();
            }
        }
        let info = probe_fn(override_path);
        *guard = Some((
            override_path.map(str::to_owned),
            Instant::now(),
            info.clone(),
        ));
        info
    }

    pub fn status_report(&self) -> AppStatusReport {
        let cfg = self.get_config();
        let (secrets_readiness, secrets_error) = match self.load_secrets() {
            Ok(s) => (s.readiness(), None),
            Err(err) => (
                SecretsReadiness {
                    vault_unreadable: true,
                    ..Default::default()
                },
                Some(err.to_string()),
            ),
        };
        let restic_bin = Self::cached_binary_probe(
            &self.restic_probe,
            cfg.restic_binary_path.as_deref(),
            probe_restic_binary,
        );
        let active_progress = lock(&self.progress).clone();
        let active_r2_delete = {
            let p = lock(&self.r2_delete_progress).clone();
            p.running.then_some(p)
        };
        let enabled_target_count = cfg.targets.iter().filter(|t| t.enabled).count();
        let active_target_count = cfg
            .targets
            .iter()
            .filter(|t| t.is_active_for_backup())
            .count();
        let next_scheduled_at = lock(&self.next_run_at).map(|dt| dt.to_rfc3339());

        let (project_sync_summary, consecutive_deferred_count, observed_remote_hosts) =
            HistoryDb::open(&self.db_path)
                .map(|db| {
                    let sync_report = sync::build_sync_report(&cfg, &db).ok();
                    let summary = sync_report
                        .as_ref()
                        .map(|r| r.summary.clone())
                        .unwrap_or_default();
                    let hosts = sync_report
                        .map(|r| r.observed_remote_hosts)
                        .unwrap_or_default();
                    let deferred = db.consecutive_scheduled_deferred_count().unwrap_or(0);
                    (summary, deferred, hosts)
                })
                .unwrap_or_default();

        AppStatusReport {
            version: env!("CARGO_PKG_VERSION").to_owned(),
            config_path: self.config_path.to_string_lossy().into_owned(),
            machine_name: cfg.effective_machine_name(),
            maintenance_role: cfg.maintenance_role,
            storage_backend: cfg.storage_backend.as_str().to_owned(),
            storage_configured: cfg.is_storage_configured(),
            repository_url: cfg.repository_url().ok(),
            r2_configured: cfg.r2.is_configured(),
            secrets_readiness,
            secrets_error,
            restic_binary: restic_bin,
            schedule_enabled: cfg.schedule.enabled,
            interval_minutes: cfg.schedule.interval_minutes,
            daily_at: cfg
                .schedule
                .daily_times()
                .into_iter()
                .map(|t| t.format("%H:%M").to_string())
                .collect(),
            maintenance_at: cfg.schedule.maintenance_at.clone(),
            next_scheduled_at,
            last_status_message: self.status_message(),
            active_progress,
            active_r2_delete,
            enabled_target_count,
            active_target_count,
            total_target_count: cfg.targets.len(),
            project_sync_summary,
            consecutive_deferred_count,
            observed_remote_hosts,
            basic_excludes: basic_list(cfg.use_basic_excludes, BASIC_EXCLUDES),
            basic_exclude_if_present: basic_list(cfg.use_basic_excludes, BASIC_EXCLUDE_IF_PRESENT),
            basic_unity_project_excludes: basic_list(
                cfg.use_basic_excludes,
                BASIC_UNITY_PROJECT_EXCLUDES,
            ),
            last_verify: self.last_verify(),
        }
    }

    /// Execute a backup job synchronously (called from worker thread, tray, or MCP handler).
    /// Creates one snapshot per target (`1 target = 1 snapshot`).
    pub fn execute_backup(
        &self,
        trigger_source: &str,
        target_ids: Option<&[String]>,
        dry_run: bool,
        extra_tags: &[String],
    ) -> AppResult<Value> {
        self.execute_backup_with_options(
            trigger_source,
            target_ids,
            None,
            dry_run,
            extra_tags,
            false,
        )
    }

    /// Scheduled backup entry point that preserves lock-timeout (`Deferred`) and standby-only
    /// (`SkippedNoActiveTargets`) distinctions for the scheduler.
    pub(crate) fn execute_scheduled_backup(&self, trigger_source: &str) -> ScheduledBackupOutcome {
        let cfg = self.get_config();
        if !cfg.has_active_backup_targets() {
            return ScheduledBackupOutcome::SkippedNoActiveTargets;
        }
        match self.execute_backup_with_options(trigger_source, None, None, false, &[], false) {
            Ok(val) => {
                if val.get("allSkipped").and_then(Value::as_bool) == Some(true) {
                    ScheduledBackupOutcome::SkippedNoActiveTargets
                } else {
                    ScheduledBackupOutcome::Success
                }
            }
            Err(AppError::ResticLockTimeout(_)) => ScheduledBackupOutcome::Deferred,
            Err(_) => ScheduledBackupOutcome::Failed,
        }
    }

    /// Execute backups one target at a time with optional `projectKeys` selection and
    /// `markHandoffReady` (`handoff:released` + switch to `standby`).
    pub fn execute_backup_with_options(
        &self,
        trigger_source: &str,
        target_ids: Option<&[String]>,
        project_keys: Option<&[String]>,
        dry_run: bool,
        extra_tags: &[String],
        mark_handoff_ready: bool,
    ) -> AppResult<Value> {
        let Ok(_job_lock) = self.job_mutex.try_lock() else {
            return Err(AppError::Restic(
                "another backup or maintenance operation is already running".to_owned(),
            ));
        };

        let cfg = self.get_config();
        let secrets = self.load_secrets()?;
        let validated_extra_tags = validate_user_tags(extra_tags)?;
        if target_ids.is_some_and(|ids| ids.is_empty()) {
            return Err(AppError::InvalidArgument(
                "targetIds must not be empty when specified".to_owned(),
            ));
        }
        if project_keys.is_some_and(|keys| keys.is_empty()) {
            return Err(AppError::InvalidArgument(
                "projectKeys must not be empty when specified".to_owned(),
            ));
        }
        let explicit_selection = target_ids.is_some() || project_keys.is_some();

        let selected_targets: Vec<BackupTarget> = if explicit_selection {
            let mut matched = Vec::new();
            if let Some(ids) = target_ids {
                for raw_id in ids {
                    let id = raw_id.trim();
                    if id.is_empty() {
                        return Err(AppError::InvalidArgument(
                            "targetIds must not contain empty entries".to_owned(),
                        ));
                    }
                    let found = cfg
                        .targets
                        .iter()
                        .find(|t| id == t.id || t.path.eq_ignore_ascii_case(id));
                    let Some(t) = found else {
                        return Err(AppError::InvalidArgument(format!(
                            "targetIds \"{id}\" に一致するバックアップ対象がありません"
                        )));
                    };
                    if !matched.iter().any(|m: &BackupTarget| m.id == t.id) {
                        matched.push(t.clone());
                    }
                }
            }
            if let Some(keys) = project_keys {
                for raw_key in keys {
                    let key = validate_project_key(raw_key)?;
                    let Some(t) = cfg.find_target_by_project_key(&key) else {
                        return Err(AppError::InvalidArgument(format!(
                            "projectKey \"{key}\" に一致するバックアップ対象がありません"
                        )));
                    };
                    if !matched.iter().any(|m| m.id == t.id) {
                        matched.push(t.clone());
                    }
                }
            }
            matched
        } else {
            cfg.targets
                .iter()
                .filter(|t| t.is_active_for_backup())
                .cloned()
                .collect()
        };

        if selected_targets.is_empty() {
            return Err(AppError::InvalidArgument(
                "この PC で作業中（active）かつ有効なバックアップ対象ディレクトリがありません"
                    .to_owned(),
            ));
        }

        if mark_handoff_ready {
            for target in &selected_targets {
                if target.project_key.is_none() {
                    return Err(AppError::InvalidArgument(format!(
                        "「{}」には projectKey が設定されていないため引き渡しできません",
                        target.label
                    )));
                }
                if is_git_repository(Path::new(&target.path)) {
                    return Err(AppError::InvalidArgument(format!(
                        "「{}」は Git 管理リポジトリのため引き渡し対象外です（GitHub で受け渡してください）",
                        target.label
                    )));
                }
                if let Some(lockfile) = find_unity_lockfile(Path::new(&target.path)) {
                    return Err(AppError::InvalidArgument(format!(
                        "Unity エディタが「{}」を開いているため中止しました（{} を検出）。Unity を閉じてからやり直してください",
                        target.label,
                        lockfile.display()
                    )));
                }
            }
        }

        let is_scheduled = matches!(trigger_source, "scheduler" | "startup");
        let retry_lock = if is_scheduled {
            RETRY_LOCK_SCHEDULED_BACKUP
        } else {
            RETRY_LOCK_MANUAL_OR_HANDOFF
        };
        let local_machine = cfg.effective_machine_name();
        let initial_storage_key = storage_destination_key(&cfg);
        let db = HistoryDb::open(&self.db_path)?;
        self.cancel_flag.store(false, Ordering::Relaxed);

        let total_targets = selected_targets.len();
        let mut results: Vec<Value> = Vec::new();
        let mut aggregate = ResticBackupSummary::default();
        let mut total_duration_ms: u64 = 0;
        let mut last_run_id: Option<i64> = None;
        let mut succeeded_labels: Vec<String> = Vec::new();
        let mut skipped_count: usize = 0;
        let mut first_error: Option<AppError> = None;

        for (idx, initial_target) in selected_targets.iter().enumerate() {
            if self.cancel_flag.load(Ordering::Relaxed) {
                first_error.get_or_insert_with(|| {
                    AppError::Restic("backup cancelled by user or agent".to_owned())
                });
                break;
            }

            // Re-read live config right before backing up this target so mid-run `standby`
            // switches or storage destination changes are honored immediately.
            let live_cfg = self.get_config();
            if storage_destination_key(&live_cfg) != initial_storage_key {
                let msg = "バックアップ実行中に保存先リポジトリ設定が変更されたため残りのバックアップを中止しました".to_owned();
                first_error.get_or_insert(AppError::InvalidArgument(msg));
                break;
            }
            let Some(live_target) = live_cfg
                .targets
                .iter()
                .find(|t| t.id == initial_target.id)
                .cloned()
            else {
                skipped_count += 1;
                continue;
            };
            if !explicit_selection && !live_target.enabled {
                skipped_count += 1;
                continue;
            }
            let target = &live_target;

            // Per-target pre-write checks right before writing THIS shared project.
            if let Some(ref key) = target.project_key {
                if target.writer_state == WriterState::Standby {
                    let reason = "待機中（standby）のため書き込めません。先に「この PC で引き継ぐ」を実行してください";
                    if explicit_selection && !dry_run {
                        return Err(AppError::InvalidArgument(format!(
                            "「{}」は{reason}",
                            target.label
                        )));
                    }
                    skipped_count += 1;
                    results.push(json!({
                        "targetId": target.id,
                        "label": target.label,
                        "projectKey": key,
                        "status": "skipped",
                        "skippedReason": reason,
                    }));
                    continue;
                }
                if is_git_repository(Path::new(&target.path)) {
                    let reason = "Git 管理リポジトリ（.git を検出）のため共有プロジェクトのバックアップを中止しました";
                    let _ = db.record_audit(
                        trigger_source,
                        "backup_skipped_git_repo",
                        &format!("project={key} path={}", target.path),
                    );
                    if explicit_selection && !dry_run {
                        return Err(AppError::InvalidArgument(format!(
                            "「{}」: {reason}",
                            target.label
                        )));
                    }
                    skipped_count += 1;
                    results.push(json!({
                        "targetId": target.id,
                        "label": target.label,
                        "projectKey": key,
                        "status": "skipped",
                        "skippedReason": reason,
                    }));
                    continue;
                }

                let existing_row = match db.get_project_sync_state(key) {
                    Ok(row) => row,
                    Err(err) => {
                        let msg = format!(
                            "同期状態の読み取りに失敗したため共有バックアップを中止しました: {err}"
                        );
                        if explicit_selection && !dry_run {
                            return Err(AppError::Restic(msg));
                        }
                        first_error.get_or_insert_with(|| AppError::Restic(msg));
                        continue;
                    }
                };
                if existing_row
                    .as_ref()
                    .is_some_and(|r| r.handoff_status != "idle")
                {
                    let reason =
                        "前回の引き継ぎが途中で中断しています。引き継ぎを再実行してください";
                    let _ = db.record_audit(
                        trigger_source,
                        "backup_skipped_interrupted_handoff",
                        &format!(
                            "project={key} status={}",
                            existing_row
                                .as_ref()
                                .map(|r| r.handoff_status.as_str())
                                .unwrap_or("unknown")
                        ),
                    );
                    if explicit_selection && !dry_run {
                        return Err(AppError::InvalidArgument(format!(
                            "「{}」は{reason}",
                            target.label
                        )));
                    }
                    skipped_count += 1;
                    results.push(json!({
                        "targetId": target.id,
                        "label": target.label,
                        "projectKey": key,
                        "status": "skipped",
                        "skippedReason": reason,
                    }));
                    continue;
                }

                let check_started = Utc::now().to_rfc3339();
                let tag_filter = format!("{TAG_PROJECT_PREFIX}{key}");
                let snaps = match runner::list_snapshots_filtered_cancellable(
                    &live_cfg,
                    &secrets,
                    None,
                    Some(&tag_filter),
                    None,
                    Some(RETRY_LOCK_READ),
                    &self.cancel_flag,
                ) {
                    Ok(snaps) => snaps,
                    Err(AppError::ResticLockTimeout(msg)) => {
                        let run_id = db.record_backup_deferred(
                            &check_started,
                            trigger_source,
                            &target.label,
                            dry_run,
                            &msg,
                        )?;
                        let _ = db.record_audit(
                            trigger_source,
                            "backup_deferred",
                            &format!("project={key} precheck {msg}"),
                        );
                        results.push(json!({
                            "targetId": target.id,
                            "label": target.label,
                            "projectKey": key,
                            "runId": run_id,
                            "status": "deferred",
                            "error": msg,
                        }));
                        self.set_status(
                            "他 PC がリポジトリを整理中のためバックアップを延期しました",
                        );
                        first_error.get_or_insert(AppError::ResticLockTimeout(msg));
                        // Continue so independent local-only targets with longer retry_lock can
                        // still attempt backup instead of being prematurely aborted.
                        continue;
                    }
                    Err(err) => {
                        let msg = format!(
                            "共有プロジェクト「{}」のリモート確認に失敗したため上書き防止のためバックアップを中止しました: {err}",
                            target.label
                        );
                        let _ = db.record_audit(
                            trigger_source,
                            "backup_skipped_precheck_failed",
                            &format!("project={key} {err}"),
                        );
                        results.push(json!({
                            "targetId": target.id,
                            "label": target.label,
                            "projectKey": key,
                            "status": "failed",
                            "error": msg,
                        }));
                        first_error.get_or_insert(AppError::Restic(msg));
                        continue;
                    }
                };
                // Evaluate remote ownership directly from the freshly fetched in-memory `snaps`
                // so a concurrent cache write can never hide a newer remote snapshot.
                let latest_eff = sync::select_latest_effective_snapshot(&snaps, key);
                let checked_at = Utc::now().to_rfc3339();
                if let Err(err) = sync::ingest_single_project_snapshots_into_db(
                    &db,
                    &local_machine,
                    key,
                    &snaps,
                    &checked_at,
                ) {
                    let msg = format!(
                        "共有プロジェクト「{}」の同期キャッシュ更新に失敗したためバックアップを中止しました: {err}",
                        target.label
                    );
                    first_error.get_or_insert(AppError::Restic(msg));
                    continue;
                }
                let last_synced_id = existing_row
                    .as_ref()
                    .and_then(|r| r.last_synced_snapshot_id.as_deref());
                if sync::is_remote_snapshot_newer_than_local(
                    &local_machine,
                    latest_eff,
                    last_synced_id,
                ) {
                    let remote_host = latest_eff
                        .map(|s| s.normalized_host())
                        .filter(|h| !h.is_empty())
                        .unwrap_or_else(|| "他 PC".to_owned());
                    let parent_id = existing_row
                        .as_ref()
                        .and_then(|r| r.last_local_backup_snapshot_id.as_deref());
                    let has_local_changes = runner::check_target_local_changes_cancellable(
                        &live_cfg,
                        &secrets,
                        target,
                        parent_id,
                        &self.cancel_flag,
                    )
                    .unwrap_or(true);
                    let _ = db.set_local_has_unbacked_changes(key, has_local_changes);
                    if !has_local_changes && !dry_run {
                        let key_clone = key.clone();
                        self.update_config(|c| {
                            if let Some(t) = c.find_target_by_project_key_mut(&key_clone) {
                                t.writer_state = WriterState::Standby;
                            }
                            Ok(())
                        })?;
                    }
                    let detail = format!(
                        "project={key} remote_host={remote_host} local_changes={has_local_changes}"
                    );
                    let _ = db.record_audit(trigger_source, "backup_skipped_remote_newer", &detail);
                    if explicit_selection && !dry_run {
                        return Err(AppError::InvalidArgument(format!(
                            "「{}」は他 PC（{remote_host}）に新しいスナップショットがあるため上書き防止のため中止しました。先に「この PC で引き継ぐ」を実行してください",
                            target.label
                        )));
                    }
                    skipped_count += 1;
                    results.push(json!({
                        "targetId": target.id,
                        "label": target.label,
                        "projectKey": key,
                        "status": "skipped",
                        "skippedReason": format!("他 PC（{remote_host}）に新しいスナップショットがあります"),
                    }));
                    continue;
                }
            }

            let progress_label = if total_targets > 1 {
                format!("[{}/{}] {}", idx + 1, total_targets, target.label)
            } else {
                target.label.clone()
            };
            let started_at = Utc::now().to_rfc3339();
            self.set_status(format!(
                "バックアップ実行中 ({}{}) ...",
                progress_label,
                if dry_run { " / Dry-Run" } else { "" }
            ));

            // For real handoff releases, stage without `project:<key>` or `handoff:released` so
            // an exit-code-3 partial snapshot is never observable by another PC.
            let staging_release = mark_handoff_ready && !dry_run && target.project_key.is_some();
            let internal_tags: Vec<String> = if staging_release {
                vec![TAG_HANDOFF_STAGING.to_owned()]
            } else if mark_handoff_ready {
                vec![TAG_HANDOFF_RELEASED.to_owned()]
            } else {
                Vec::new()
            };

            let opts = BackupTargetOptions {
                dry_run,
                extra_tags: &validated_extra_tags,
                internal_tags: &internal_tags,
                suppress_project_tag: staging_release,
                retry_lock,
                parent_snapshot: None,
                progress_label: Some(&progress_label),
            };

            match runner::run_backup_target(
                &live_cfg,
                &secrets,
                target,
                &opts,
                &self.progress,
                &self.cancel_flag,
            ) {
                Ok((mut summary, elapsed_ms)) => {
                    if staging_release
                        && (summary.partial_read_warning || summary.snapshot_id.is_none())
                    {
                        let cleanup_note = summary
                            .snapshot_id
                            .as_deref()
                            .map(|id| cleanup_staging_snapshot_note(&live_cfg, &secrets, id))
                            .unwrap_or_default();
                        let msg = format!(
                            "「{}」の引き渡しバックアップで読み取れないファイルがあったため（終了コード 3）、不完全なスナップショットを公開せず中止しました: {}{cleanup_note}",
                            target.label,
                            summary.unreadable.join("; ")
                        );
                        let _ = db.record_backup_run(
                            &started_at,
                            trigger_source,
                            &target.label,
                            false,
                            Err(&msg),
                        );
                        let _ = db.record_audit(trigger_source, "run_backup_failed", &msg);
                        first_error.get_or_insert_with(|| AppError::Restic(msg));
                        continue;
                    }

                    // For handoff releases: persist `"releasing"` checkpoint and switch Standby
                    // before promoting the staging snapshot, and never roll back Standby once
                    // `restic tag` itself succeeded.
                    if staging_release && let Some(ref key) = target.project_key {
                        let staging_id = summary.snapshot_id.clone().ok_or_else(|| {
                            AppError::Restic("missing staging snapshot id".to_owned())
                        })?;
                        db.set_handoff_state(
                            key,
                            "releasing",
                            Some(&staging_id),
                            Some(&target.path),
                            None,
                        )?;
                        let key_clone = key.clone();
                        let expected_storage = initial_storage_key.clone();
                        if let Err(cfg_err) = self.update_config(|c| {
                            if storage_destination_key(c) != expected_storage {
                                return Err(AppError::InvalidArgument(
                                    "保存先設定が変更されたため引き渡し公開を中止しました"
                                        .to_owned(),
                                ));
                            }
                            if let Some(t) = c.find_target_by_project_key_mut(&key_clone) {
                                t.writer_state = WriterState::Standby;
                            }
                            Ok(())
                        }) {
                            let _ = db.set_handoff_state(key, "idle", None, None, None);
                            let note =
                                cleanup_staging_snapshot_note(&live_cfg, &secrets, &staging_id);
                            let msg = format!(
                                "ローカルの待機中設定保存に失敗したため引き渡し公開を中止しました: {cfg_err}{note}"
                            );
                            let _ = db.record_backup_run(
                                &started_at,
                                trigger_source,
                                &target.label,
                                false,
                                Err(&msg),
                            );
                            first_error.get_or_insert(AppError::Restic(msg));
                            continue;
                        }
                        match runner::promote_handoff_snapshot(
                            &live_cfg,
                            &secrets,
                            &staging_id,
                            key,
                            TAG_HANDOFF_RELEASED,
                            &[],
                            retry_lock,
                            &self.cancel_flag,
                        ) {
                            Ok(PromoteHandoffOutcome::Confirmed { snapshot_id, .. }) => {
                                let _ = db.set_handoff_state(key, "idle", None, None, None);
                                summary.snapshot_id = Some(snapshot_id);
                            }
                            Ok(PromoteHandoffOutcome::NotFoundAfterLookup) => {
                                // Successful listing proved nothing was published (e.g. Restic
                                // `cmd_tag` per-snapshot warning with exit 0); safe to roll back.
                                let _ = self.update_config(|c| {
                                    if let Some(t) = c.find_target_by_project_key_mut(&key_clone) {
                                        t.writer_state = WriterState::Active;
                                    }
                                    Ok(())
                                });
                                let _ = db.set_handoff_state(key, "idle", None, None, None);
                                let msg = format!(
                                    "引き渡しタグの公開後に公開済みスナップショットを確認できなかったため中止しました（staging={staging_id}）"
                                );
                                let _ = db.record_backup_run(
                                    &started_at,
                                    trigger_source,
                                    &target.label,
                                    false,
                                    Err(&msg),
                                );
                                first_error.get_or_insert(AppError::Restic(msg));
                                continue;
                            }
                            Ok(PromoteHandoffOutcome::LookupUncertain(lookup_err)) => {
                                // Keep Standby + `"releasing"` so retry can confirm/finish without
                                // risking overwriting an already-published release.
                                let msg = format!(
                                    "引き渡しタグ公開後の確認が中断・失敗しました（重複書き込み防止のため待機中・引き渡し中断状態を維持します。再度「引き渡しを再開」を実行してください）: {lookup_err}"
                                );
                                let _ = db.record_audit(
                                    trigger_source,
                                    "release_project_uncertain",
                                    &format!("project={key} staging={staging_id} {lookup_err}"),
                                );
                                first_error.get_or_insert(AppError::Restic(msg));
                                continue;
                            }
                            Err(tag_err) => {
                                // Keep Standby + `"releasing"` instead of blindly rolling back Active
                                // in case `restic tag` committed right before cancellation/signal.
                                let msg = format!(
                                    "引き渡しタグの公開が中断または失敗しました（再実行で安全に復旧できます）: {tag_err}"
                                );
                                let _ = db.record_backup_run(
                                    &started_at,
                                    trigger_source,
                                    &target.label,
                                    false,
                                    Err(&msg),
                                );
                                first_error.get_or_insert(AppError::Restic(msg));
                                continue;
                            }
                        }
                    }

                    let run_id = db.record_backup_run(
                        &started_at,
                        trigger_source,
                        &target.label,
                        dry_run,
                        Ok(&(summary.clone(), elapsed_ms)),
                    )?;
                    let detail = format!(
                        "target={} project={:?} dry_run={dry_run} snapshot={:?} added={}B unreadable={}",
                        target.label,
                        target.project_key,
                        summary.snapshot_id,
                        summary.data_added,
                        summary.unreadable.len()
                    );
                    let _ = db.record_audit(trigger_source, "run_backup", &detail);

                    if !dry_run
                        && let Some(ref key) = target.project_key
                        && let Some(ref snap_id) = summary.snapshot_id
                    {
                        let now_iso = Utc::now().to_rfc3339();
                        db.update_local_backup_sync(
                            key,
                            snap_id,
                            &now_iso,
                            &local_machine,
                            mark_handoff_ready,
                            &target.path,
                            summary.exclude_fingerprint.as_deref(),
                        )?;
                        let _ = db.set_last_remote_checked_at(&now_iso);
                    }

                    last_run_id = Some(run_id);
                    total_duration_ms += elapsed_ms;
                    aggregate.files_new += summary.files_new;
                    aggregate.files_changed += summary.files_changed;
                    aggregate.files_unmodified += summary.files_unmodified;
                    aggregate.dirs_new += summary.dirs_new;
                    aggregate.dirs_changed += summary.dirs_changed;
                    aggregate.dirs_unmodified += summary.dirs_unmodified;
                    aggregate.data_added += summary.data_added;
                    aggregate.total_files_processed += summary.total_files_processed;
                    aggregate.total_bytes_processed += summary.total_bytes_processed;
                    aggregate.total_duration += summary.total_duration;
                    aggregate.snapshot_id = summary.snapshot_id.clone();
                    aggregate.unreadable.extend(summary.unreadable.clone());
                    succeeded_labels.push(target.label.clone());

                    results.push(json!({
                        "targetId": target.id,
                        "label": target.label,
                        "projectKey": target.project_key,
                        "runId": run_id,
                        "status": "success",
                        "snapshotId": summary.snapshot_id,
                        "summary": summary,
                        "durationMs": elapsed_ms,
                    }));
                }
                Err(AppError::ResticLockTimeout(msg)) => {
                    let run_id = db.record_backup_deferred(
                        &started_at,
                        trigger_source,
                        &target.label,
                        dry_run,
                        &msg,
                    )?;
                    let _ = db.record_audit(
                        trigger_source,
                        "backup_deferred",
                        &format!("target={} {msg}", target.label),
                    );
                    results.push(json!({
                        "targetId": target.id,
                        "label": target.label,
                        "projectKey": target.project_key,
                        "runId": run_id,
                        "status": "deferred",
                        "error": msg,
                    }));
                    self.set_status(format!(
                        "他 PC がリポジトリを整理中のためバックアップを延期しました ({})",
                        target.label
                    ));
                    first_error = Some(AppError::ResticLockTimeout(msg));
                    break;
                }
                Err(err) => {
                    let msg = err.to_string();
                    let run_id = db.record_backup_run(
                        &started_at,
                        trigger_source,
                        &target.label,
                        dry_run,
                        Err(&msg),
                    )?;
                    let _ = db.record_audit(
                        trigger_source,
                        "run_backup_failed",
                        &format!("target={} {msg}", target.label),
                    );
                    results.push(json!({
                        "targetId": target.id,
                        "label": target.label,
                        "projectKey": target.project_key,
                        "runId": run_id,
                        "status": "failed",
                        "error": msg,
                    }));
                    first_error.get_or_insert(err);
                }
            }
        }

        if let Some(err) = first_error {
            if !err.is_lock_timeout() {
                self.set_status(format!("バックアップ失敗: {err}"));
            }
            return Err(err);
        }

        if succeeded_labels.is_empty() && skipped_count > 0 {
            self.set_status("バックアップ対象はすべてスキップされました（他 PC が最新）");
            return Ok(json!({
                "dryRun": dry_run,
                "allSkipped": true,
                "targets": "",
                "results": results,
            }));
        }

        // Legacy single-PC auto-prune: only runs when maintenanceRole is Primary, no dedicated
        // maintenanceAt window is configured, this is not a handoff release backup, and (for
        // MCP-initiated backups) `agent.allowPrune` permits agent-triggered snapshot deletion.
        let prune_error = (!dry_run
            && !mark_handoff_ready
            && cfg.maintenance_role == MaintenanceRole::Primary
            && cfg.schedule.auto_prune_after_backup
            && cfg.schedule.maintenance_at.is_none()
            && (trigger_source != "mcp" || cfg.agent.allow_prune))
            .then(|| {
                match runner::forget_and_prune_with_retry(
                    &cfg,
                    &secrets,
                    false,
                    RETRY_LOCK_MAINTENANCE,
                ) {
                    Ok(_) => {
                        let _ = db.record_audit(trigger_source, "forget_prune", "dry_run=false");
                        None
                    }
                    Err(AppError::ResticLockTimeout(msg)) => {
                        let _ = db.record_audit(trigger_source, "forget_prune_deferred", &msg);
                        None
                    }
                    Err(err) => {
                        let msg = err.to_string();
                        let _ = db.record_audit(trigger_source, "forget_prune_failed", &msg);
                        Some(msg)
                    }
                }
            })
            .flatten();

        let target_summary = succeeded_labels.join(", ");
        self.set_status(format!(
            "バックアップ完了: {} (新規 {} / 変更 {} ファイル, Snapshot: {}){}{}",
            target_summary,
            aggregate.files_new,
            aggregate.files_changed,
            aggregate.snapshot_id.as_deref().unwrap_or("dry-run"),
            if aggregate.unreadable.is_empty() {
                String::new()
            } else {
                format!(" 読み取れなかった項目 {} 件", aggregate.unreadable.len())
            },
            if prune_error.is_some() {
                " 古い世代の整理に失敗しました"
            } else {
                ""
            }
        ));

        let mut result = json!({
            "runId": last_run_id,
            "dryRun": dry_run,
            "targets": target_summary,
            "summary": aggregate,
            "durationMs": total_duration_ms,
            "results": results,
        });
        if let Some(msg) = prune_error {
            result["pruneError"] = json!(msg);
        }
        Ok(result)
    }

    fn last_verify(&self) -> Option<VerifyStatus> {
        let entry = HistoryDb::open(&self.db_path)
            .ok()?
            .last_audit_of("verify_repository")
            .ok()??;
        Some(VerifyStatus {
            at: entry.timestamp,
            ok: entry.operation == "verify_repository",
            detail: entry.detail,
        })
    }

    /// Check the repository and read back part of its data (`percent`, or the configured
    /// share), recorded under `actor`. A failure (other than scheduled lock deferral)
    /// also raises a Windows notification, because this mostly runs with the window hidden.
    pub fn verify_repository(&self, actor: &str, percent: Option<u32>) -> AppResult<String> {
        self.verify_repository_inner(actor, percent, false)
    }

    fn verify_repository_inner(
        &self,
        actor: &str,
        percent: Option<u32>,
        is_scheduled: bool,
    ) -> AppResult<String> {
        let Ok(_job_lock) = self.job_mutex.try_lock() else {
            return Err(AppError::Restic(
                "another backup or maintenance operation is already running".to_owned(),
            ));
        };
        let cfg = self.get_config();
        let secrets = self.load_secrets()?;
        let percent = percent
            .unwrap_or(cfg.schedule.verify_read_percent)
            .clamp(1, 100);
        self.set_status(format!(
            "復元できるか確認中 (データの {percent}% を読み出し中) ..."
        ));
        let result = runner::verify_repository_with_retry(
            &cfg,
            &secrets,
            percent,
            Some(RETRY_LOCK_MAINTENANCE),
        );
        match &result {
            Ok(_) => {
                self.record_audit(
                    actor,
                    "verify_repository",
                    &format!("read {percent}% of the data, no errors"),
                );
                self.set_status("復元できることを確認しました。問題はありませんでした");
            }
            Err(AppError::ResticLockTimeout(msg)) if is_scheduled => {
                self.record_audit(actor, "verify_repository_deferred", msg);
                self.set_status(
                    "他 PC がリポジトリを使用中のため復元確認を次回メンテナンス枠へ持ち越しました",
                );
            }
            Err(err) => {
                let msg = err.to_string();
                self.record_audit(actor, "verify_repository_failed", &msg);
                self.set_status(format!("復元の確認で問題が見つかりました: {msg}"));
                notify::toast(
                    "バックアップデータの確認で問題が見つかりました",
                    "保存したデータを読み戻せない可能性があります。アプリを開いて内容を確認してください。",
                );
            }
        }
        result
    }

    /// Runs `forget --prune` (and due `verify`) when this PC has `maintenanceRole: "primary"`.
    /// When `schedule.maintenance_at` is set, runs at most once per calendar day on the matching
    /// maintenance slot (never during other daily slots like 17:00), and marks today's slot as
    /// attempted even on lock timeout (exit code 11) so standby primary PCs wait until tomorrow.
    pub(crate) fn run_due_maintenance_if_primary(&self, allow_maintenance_slot: bool) {
        let cfg = self.get_config();
        if cfg.maintenance_role != MaintenanceRole::Primary {
            return;
        }
        let Ok(db) = HistoryDb::open(&self.db_path) else {
            return;
        };
        let now_local = Local::now();
        let now_utc = now_local.with_timezone(&Utc);

        if let Some(slot) = cfg.schedule.maintenance_time() {
            if !allow_maintenance_slot {
                return;
            }
            if now_local.time() < slot {
                return;
            }
            let today_ymd = now_local.format("%Y-%m-%d").to_string();
            if db.last_auto_maintenance_date().ok().flatten().as_deref() == Some(today_ymd.as_str())
            {
                return;
            }
            let last_auto_prune = db
                .last_audit_exact("forget_prune_auto")
                .ok()
                .flatten()
                .and_then(|e| DateTime::parse_from_rfc3339(&e.timestamp).ok())
                .map(|at| at.with_timezone(&Utc));
            if !maintenance_is_due(last_auto_prune, &now_local, slot) {
                return;
            }
            // Acquire the local job lock before stamping today's date so an in-flight manual
            // backup never consumes today's maintenance window without running Restic.
            if !self.forget_prune_scheduled_for_date(&db, &today_ymd) {
                return;
            }
            let last_verify = db
                .last_audit_of("verify_repository")
                .ok()
                .flatten()
                .and_then(|entry| DateTime::parse_from_rfc3339(&entry.timestamp).ok())
                .map(|at| at.with_timezone(&Utc));
            if verify_is_due(last_verify, now_utc, cfg.schedule.verify_every_days) {
                let _ = self.verify_repository_inner("scheduler", None, true);
            }
        } else if allow_maintenance_slot {
            let last_verify = db
                .last_audit_of("verify_repository")
                .ok()
                .flatten()
                .and_then(|entry| DateTime::parse_from_rfc3339(&entry.timestamp).ok())
                .map(|at| at.with_timezone(&Utc));
            if verify_is_due(last_verify, now_utc, cfg.schedule.verify_every_days) {
                let _ = self.verify_repository_inner("scheduler", None, true);
            }
        }
    }

    /// Runs scheduled `forget --prune` after acquiring `job_mutex`. Returns `false` if another
    /// local job is currently running (without consuming `today_ymd`), or `true` once the
    /// maintenance attempt ran (stamping `today_ymd` even on remote lock timeout exit code 11 so
    /// standby primary PCs wait until tomorrow's slot).
    fn forget_prune_scheduled_for_date(&self, db: &HistoryDb, today_ymd: &str) -> bool {
        let Ok(_job_lock) = self.job_mutex.try_lock() else {
            return false;
        };
        let cfg = self.get_config();
        let Ok(secrets) = self.load_secrets() else {
            return false;
        };
        let _ = db.set_last_auto_maintenance_date(today_ymd);
        self.set_status("古いスナップショットを整理中 (forget --prune) ...");
        match runner::forget_and_prune_with_retry(&cfg, &secrets, false, RETRY_LOCK_MAINTENANCE) {
            Ok(out) => {
                let detail = summarize_forget_audit(&out, false, true);
                self.record_audit("scheduler", "forget_prune_auto", &detail);
                self.record_audit("scheduler", "forget_prune", &detail);
                self.set_status("古いスナップショットの整理が完了しました");
            }
            Err(AppError::ResticLockTimeout(msg)) => {
                self.record_audit("scheduler", "forget_prune_deferred", &msg);
                self.set_status("他 PC が使用中のため整理を次回メンテナンス枠へ持ち越しました");
            }
            Err(err) => {
                let msg = err.to_string();
                self.record_audit("scheduler", "forget_prune_failed", &msg);
                self.set_status(format!("古い世代の整理に失敗しました: {msg}"));
            }
        }
        true
    }

    /// After a scheduled backup attempt: run due maintenance on primary PCs (only when
    /// `allow_maintenance_slot` matches the configured slot), track consecutive scheduled
    /// lock deferrals (notify only at 3+), and suppress overdue warnings when standby-only.
    pub(crate) fn after_scheduled_backup(
        &self,
        outcome: ScheduledBackupOutcome,
        allow_maintenance_slot: bool,
    ) {
        let cfg = self.get_config();
        let Ok(db) = HistoryDb::open(&self.db_path) else {
            return;
        };
        let now = Utc::now();

        match outcome {
            ScheduledBackupOutcome::SkippedNoActiveTargets => {
                // Standby-only state on a secondary PC is healthy; suppress overdue notifications.
                let _ = db.set_consecutive_scheduled_deferred_count(0);
                self.deferred_notified.store(false, Ordering::Relaxed);
                self.overdue_notified.store(false, Ordering::Relaxed);
                if allow_maintenance_slot {
                    self.run_due_maintenance_if_primary(true);
                }
                return;
            }
            ScheduledBackupOutcome::Success => {
                let _ = db.set_consecutive_scheduled_deferred_count(0);
                self.deferred_notified.store(false, Ordering::Relaxed);
                self.run_due_maintenance_if_primary(allow_maintenance_slot);
            }
            ScheduledBackupOutcome::Deferred => {
                let consecutive = db
                    .consecutive_scheduled_deferred_count()
                    .unwrap_or(0)
                    .saturating_add(1);
                let _ = db.set_consecutive_scheduled_deferred_count(consecutive);
                if consecutive >= 3 && !self.deferred_notified.swap(true, Ordering::Relaxed) {
                    notify::toast(
                        "リポジトリがロックされたままです",
                        "他 PC の整理処理または古いロックによりバックアップが連続して延期されています。設定画面からロック解除を確認してください。",
                    );
                }
                return;
            }
            ScheduledBackupOutcome::Failed => {
                let _ = db.set_consecutive_scheduled_deferred_count(0);
                self.deferred_notified.store(false, Ordering::Relaxed);
            }
        }

        if !cfg.has_active_backup_targets() {
            self.overdue_notified.store(false, Ordering::Relaxed);
            return;
        }

        if !backup_is_overdue(last_success_at(&db), now, cfg.schedule.notify_overdue_hours) {
            self.overdue_notified.store(false, Ordering::Relaxed);
        } else if !self.overdue_notified.swap(true, Ordering::Relaxed) {
            notify::toast(
                "バックアップが成功していません",
                &format!(
                    "{} 時間以上バックアップが成功していません。アプリを開いて状況を確認してください。",
                    cfg.schedule.notify_overdue_hours
                ),
            );
        }
    }

    /// Runs safe `restic unlock` (never `--remove-all`) to clear stale locks older than 30 minutes.
    pub fn unlock_repository(&self, actor: &str) -> AppResult<String> {
        let Ok(_job_lock) = self.job_mutex.try_lock() else {
            return Err(AppError::Restic(
                "another backup or maintenance operation is already running".to_owned(),
            ));
        };
        let cfg = self.get_config();
        let secrets = self.load_secrets()?;
        self.set_status("古いロックを解除しています (restic unlock) ...");
        let result = runner::unlock_repository(&cfg, &secrets);
        match &result {
            Ok(out) => {
                self.record_audit(actor, "unlock_repository", out);
                self.set_status("古いロックを確認・解除しました");
            }
            Err(err) => {
                let msg = err.to_string();
                self.record_audit(actor, "unlock_repository_failed", &msg);
                self.set_status(format!("ロック解除に失敗しました: {msg}"));
            }
        }
        result
    }

    /// Runs `forget --prune` (or dry-run) with multi-PC per-project grouping and safety-backup
    /// protection, recording the result in the audit log.
    pub fn forget_prune(&self, actor: &str, dry_run: bool) -> AppResult<String> {
        let Ok(_job_lock) = self.job_mutex.try_lock() else {
            return Err(AppError::Restic(
                "another backup or maintenance operation is already running".to_owned(),
            ));
        };
        let cfg = self.get_config();
        let secrets = self.load_secrets()?;
        self.set_status(if dry_run {
            "古い世代の整理対象を確認中 (Dry-Run) ..."
        } else {
            "古いスナップショットを整理中 (forget --prune) ..."
        });
        let result =
            runner::forget_and_prune_with_retry(&cfg, &secrets, dry_run, RETRY_LOCK_MAINTENANCE);
        match &result {
            Ok(out) => {
                let op_name = if dry_run {
                    "forget_prune_dry_run"
                } else {
                    "forget_prune"
                };
                let detail = summarize_forget_audit(out, dry_run, false);
                self.record_audit(actor, op_name, &detail);
                self.set_status(if dry_run {
                    "整理対象の確認が完了しました"
                } else {
                    "古いスナップショットの整理が完了しました"
                });
            }
            Err(AppError::ResticLockTimeout(msg)) => {
                self.record_audit(actor, "forget_prune_deferred", msg);
                self.set_status("他 PC がリポジトリを使用中のため整理を実行できませんでした");
            }
            Err(err) => {
                let msg = err.to_string();
                self.record_audit(actor, "forget_prune_failed", &msg);
                self.set_status(format!("古い世代の整理に失敗しました: {msg}"));
            }
        }
        result
    }

    /// Deletes a single snapshot by ID (intended for manual removal of `handoff:safety-backup`).
    pub fn forget_snapshot(&self, actor: &str, snapshot_id: &str) -> AppResult<String> {
        let Ok(_job_lock) = self.job_mutex.try_lock() else {
            return Err(AppError::Restic(
                "another backup or maintenance operation is already running".to_owned(),
            ));
        };
        let cfg = self.get_config();
        let secrets = self.load_secrets()?;
        let result = runner::forget_snapshot(&cfg, &secrets, snapshot_id);
        match &result {
            Ok(_) => {
                self.record_audit(actor, "forget_snapshot", &format!("snapshot={snapshot_id}"));
            }
            Err(err) => {
                self.record_audit(
                    actor,
                    "forget_snapshot_failed",
                    &format!("snapshot={snapshot_id} err={err}"),
                );
            }
        }
        result
    }

    /// Refreshes the SQLite multi-PC project sync cache by querying `restic snapshots --json`.
    pub fn refresh_project_sync_status(
        &self,
        actor: &str,
        check_local_changes: bool,
    ) -> AppResult<ProjectSyncReport> {
        let cfg = self.get_config();
        let secrets = self.load_secrets()?;
        let snaps = runner::list_snapshots_filtered(
            &cfg,
            &secrets,
            None,
            None,
            None,
            Some(RETRY_LOCK_READ),
        )?;
        let db = HistoryDb::open(&self.db_path)?;
        let checked_at = Utc::now().to_rfc3339();
        let local_machine = cfg.effective_machine_name();
        let configured_keys: Vec<String> = cfg
            .targets
            .iter()
            .filter_map(|t| t.project_key.clone())
            .collect();
        sync::ingest_snapshots_into_db(&db, &local_machine, &snaps, &checked_at, &configured_keys)?;

        if check_local_changes {
            self.update_local_change_flags(&cfg, &secrets, &db, &local_machine)?;
        }

        self.record_audit(
            actor,
            "refresh_sync_status",
            &format!(
                "snapshots={} checkLocalChanges={check_local_changes}",
                snaps.len()
            ),
        );
        sync::build_sync_report(&cfg, &db)
    }

    fn update_local_change_flags(
        &self,
        cfg: &AppConfig,
        secrets: &StoredSecrets,
        db: &HistoryDb,
        local_machine: &str,
    ) -> AppResult<()> {
        for target in &cfg.targets {
            let Some(ref key) = target.project_key else {
                continue;
            };
            let Some(row) = db.get_project_sync_state(key)? else {
                continue;
            };
            if !sync::is_remote_newer(local_machine, &row) || !Path::new(&target.path).is_dir() {
                continue;
            }
            let has_changes = if row.last_synced_snapshot_id.is_none() {
                true
            } else {
                runner::check_target_local_changes(
                    cfg,
                    secrets,
                    target,
                    row.last_local_backup_snapshot_id.as_deref(),
                )
                .unwrap_or(true)
            };
            if row.local_has_unbacked_changes != has_changes {
                db.set_local_has_unbacked_changes(key, has_changes)?;
            }
        }
        Ok(())
    }

    /// Returns cached or freshly refreshed multi-PC project synchronization status.
    pub fn get_project_sync_status(
        &self,
        actor: &str,
        project_key: Option<&str>,
        refresh: bool,
        check_local_changes: bool,
    ) -> AppResult<ProjectSyncReport> {
        let mut report = if refresh {
            self.refresh_project_sync_status(actor, check_local_changes)?
        } else {
            let cfg = self.get_config();
            let db = HistoryDb::open(&self.db_path)?;
            if check_local_changes {
                let secrets = self.load_secrets()?;
                let local_machine = cfg.effective_machine_name();
                self.update_local_change_flags(&cfg, &secrets, &db, &local_machine)?;
            }
            sync::build_sync_report(&cfg, &db)?
        };

        if let Some(raw_key) = project_key.map(str::trim).filter(|s| !s.is_empty()) {
            let key = validate_project_key(raw_key)?;
            report
                .projects
                .retain(|p| p.project_key.as_deref() == Some(key.as_str()));
        }
        Ok(report)
    }

    /// Releases a shared project on this PC: checks `active` state, `.git`, `UnityLockfile`,
    /// interrupted handoff, and fresh remote state, creates a clean `project:<key>` +
    /// `handoff:released` snapshot (aborting and cleaning up on exit code 3), and sets
    /// `writerState = "standby"`.
    pub fn release_project(
        &self,
        actor: &str,
        project_key: &str,
        dry_run: bool,
    ) -> AppResult<Value> {
        let key = validate_project_key(project_key)?;
        let cfg = self.get_config();
        let Some(target) = cfg.find_target_by_project_key(&key).cloned() else {
            return Err(AppError::InvalidArgument(format!(
                "projectKey \"{key}\" に一致する共有プロジェクトがこの PC に登録されていません"
            )));
        };

        let target_path = Path::new(&target.path);
        if !target_path.is_dir() {
            return Err(AppError::InvalidArgument(format!(
                "対象フォルダが見つかりません: {}",
                target.path
            )));
        }
        if is_git_repository(target_path) {
            return Err(AppError::InvalidArgument(
                "Git 管理リポジトリは引き渡し対象外です（GitHub で受け渡してください）".to_owned(),
            ));
        }
        if let Some(lockfile) = find_unity_lockfile(target_path) {
            return Err(AppError::InvalidArgument(format!(
                "Unity エディタがプロジェクトを開いているため引き渡しを中止しました（{} を検出）。Unity を閉じてからやり直してください",
                lockfile.display()
            )));
        }

        let Ok(_job_lock) = self.job_mutex.try_lock() else {
            return Err(AppError::Restic(
                "another backup or maintenance operation is already running".to_owned(),
            ));
        };

        let secrets = self.load_secrets()?;
        let local_machine = cfg.effective_machine_name();
        let initial_storage_key = storage_destination_key(&cfg);
        let db = HistoryDb::open(&self.db_path)?;
        self.cancel_flag.store(false, Ordering::Relaxed);

        let existing_row = db.get_project_sync_state(&key)?;
        let resuming_release = existing_row
            .as_ref()
            .is_some_and(|r| r.handoff_status == "releasing");
        if target.writer_state == WriterState::Standby && !resuming_release {
            return Err(AppError::InvalidArgument(format!(
                "「{}」は既に待機中（standby）のため引き渡しできません。この PC で作業を再開する場合は先に「この PC で引き継ぐ」を実行してください",
                target.label
            )));
        }
        if existing_row
            .as_ref()
            .is_some_and(|r| r.handoff_status != "idle" && r.handoff_status != "releasing")
        {
            return Err(AppError::InvalidArgument(format!(
                "「{}」は前回の引き継ぎが途中で中断しているため引き渡しできません。先に「この PC で引き継ぐ」を再実行してください",
                target.label
            )));
        }

        // Fresh remote check immediately before writing, evaluated directly on in-memory snapshots.
        let tag_filter = format!("{TAG_PROJECT_PREFIX}{key}");
        let remote_snaps = runner::list_snapshots_filtered_cancellable(
            &cfg,
            &secrets,
            None,
            Some(&tag_filter),
            None,
            Some(RETRY_LOCK_MANUAL_OR_HANDOFF),
            &self.cancel_flag,
        )?;
        let latest_eff = sync::select_latest_effective_snapshot(&remote_snaps, &key);
        let checked_at = Utc::now().to_rfc3339();
        sync::ingest_single_project_snapshots_into_db(
            &db,
            &local_machine,
            &key,
            &remote_snaps,
            &checked_at,
        )?;
        let last_synced_id = existing_row
            .as_ref()
            .and_then(|r| r.last_synced_snapshot_id.as_deref());
        if sync::is_remote_snapshot_newer_than_local(&local_machine, latest_eff, last_synced_id) {
            if resuming_release {
                let _ = db.set_handoff_state(&key, "idle", None, None, None);
            }
            let remote_host = latest_eff
                .map(|s| s.normalized_host())
                .filter(|h| !h.is_empty())
                .unwrap_or_else(|| "他 PC".to_owned());
            return Err(AppError::InvalidArgument(format!(
                "「{}」は他 PC（{remote_host}）に新しいスナップショットがあるため引き渡しを中止しました。先に「この PC で引き継ぐ」を実行してください",
                target.label
            )));
        }
        // Crash recovery for `"releasing"`: if a prior release already promoted its staging
        // snapshot before exiting, finalize cleanly without re-uploading. If NOT yet published,
        // keep `prev_staging` intact until AFTER the new release succeeds so an interrupted
        // initial release never destroys the only complete snapshot before a replacement exists!
        let mut prev_staging_to_cleanup: Option<String> = None;
        if resuming_release
            && !dry_run
            && let Some(prev_staging) = existing_row
                .as_ref()
                .and_then(|r| r.handoff_source_snapshot_id.as_deref())
        {
            if let Some(already_promoted) = remote_snaps
                .iter()
                .find(|s| s.original.as_deref() == Some(prev_staging))
            {
                let now_iso = Utc::now().to_rfc3339();
                let _ = db.update_local_backup_sync(
                    &key,
                    &already_promoted.id,
                    &now_iso,
                    &local_machine,
                    true,
                    &target.path,
                    already_promoted.exclude_fingerprint(),
                );
                let _ = db.set_handoff_state(&key, "idle", None, None, None);
                self.set_status(format!(
                    "「{}」を引き渡し済みにしました（待機中）",
                    target.label
                ));
                return Ok(json!({
                    "projectKey": key,
                    "label": target.label,
                    "dryRun": false,
                    "snapshotId": already_promoted.id,
                    "writerState": WriterState::Standby,
                    "recoveredFromPreviousRelease": true,
                }));
            }
            prev_staging_to_cleanup = Some(prev_staging.to_owned());
        }

        let started_at = Utc::now().to_rfc3339();
        self.set_status(format!(
            "引き渡しバックアップを実行中 ({}{}) ...",
            target.label,
            if dry_run { " / Dry-Run" } else { "" }
        ));

        // Stage real release backup without `project:<key>` or `handoff:released` so an
        // exit-code-3 partial snapshot is never visible to other PCs.
        let staging_release = !dry_run;
        let internal_tags = if staging_release {
            vec![TAG_HANDOFF_STAGING.to_owned()]
        } else {
            vec![TAG_HANDOFF_RELEASED.to_owned()]
        };
        let opts = BackupTargetOptions {
            dry_run,
            extra_tags: &[],
            internal_tags: &internal_tags,
            suppress_project_tag: staging_release,
            retry_lock: RETRY_LOCK_MANUAL_OR_HANDOFF,
            parent_snapshot: None,
            progress_label: Some(&target.label),
        };

        match runner::run_backup_target(
            &cfg,
            &secrets,
            &target,
            &opts,
            &self.progress,
            &self.cancel_flag,
        ) {
            Ok((mut summary, elapsed_ms)) => {
                if !dry_run && (summary.partial_read_warning || summary.snapshot_id.is_none()) {
                    let cleanup_note = summary
                        .snapshot_id
                        .as_deref()
                        .map(|id| cleanup_staging_snapshot_note(&cfg, &secrets, id))
                        .unwrap_or_default();
                    let msg = format!(
                        "引き渡しバックアップで読み取れないファイルがあったため（終了コード 3）、不完全なスナップショットを公開せず中止しました: {}{cleanup_note}",
                        summary.unreadable.join("; ")
                    );
                    let _ =
                        db.record_backup_run(&started_at, actor, &target.label, false, Err(&msg));
                    let _ = db.record_audit(
                        actor,
                        "release_project_failed",
                        &format!("project={key} {msg}"),
                    );
                    self.set_status(&msg);
                    return Err(AppError::Restic(msg));
                }

                let mut writer_state = target.writer_state;
                if !dry_run {
                    let staging_id = summary.snapshot_id.clone().ok_or_else(|| {
                        AppError::Restic("missing staging snapshot id".to_owned())
                    })?;
                    // Persist `"releasing"` checkpoint before setting Standby so if the process
                    // terminates before `restic tag`, retrying `release_project` can recover.
                    db.set_handoff_state(
                        &key,
                        "releasing",
                        Some(&staging_id),
                        Some(&target.path),
                        None,
                    )?;
                    // Switch local config to Standby first (while verifying storage destination
                    // has not changed), before promoting the snapshot to `handoff:released`.
                    let expected_storage = initial_storage_key.clone();
                    if let Err(cfg_err) = self.update_config(|c| {
                        if storage_destination_key(c) != expected_storage {
                            return Err(AppError::InvalidArgument(
                                "引き渡し中に保存先リポジトリ設定が変更されたため公開を中止しました"
                                    .to_owned(),
                            ));
                        }
                        if let Some(t) = c.find_target_by_project_key_mut(&key) {
                            t.writer_state = WriterState::Standby;
                        }
                        Ok(())
                    }) {
                        let _ = db.set_handoff_state(&key, "idle", None, None, None);
                        let note = cleanup_staging_snapshot_note(&cfg, &secrets, &staging_id);
                        let msg = format!(
                            "待機中状態の保存に失敗したため引き渡し公開を中止しました: {cfg_err}{note}"
                        );
                        let _ = db.record_audit(
                            actor,
                            "release_project_failed",
                            &format!("project={key} {msg}"),
                        );
                        self.set_status(&msg);
                        return Err(AppError::Restic(msg));
                    }

                    let promoted_id = match runner::promote_handoff_snapshot(
                        &cfg,
                        &secrets,
                        &staging_id,
                        &key,
                        TAG_HANDOFF_RELEASED,
                        &[],
                        RETRY_LOCK_MANUAL_OR_HANDOFF,
                        &self.cancel_flag,
                    ) {
                        Ok(PromoteHandoffOutcome::Confirmed { snapshot_id, .. }) => snapshot_id,
                        Ok(PromoteHandoffOutcome::NotFoundAfterLookup) => {
                            // Successful listing proved nothing was published (Restic `cmd_tag`
                            // partial-failure exit 0 path); safe to roll back Active and report error.
                            let _ = self.update_config(|c| {
                                if let Some(t) = c.find_target_by_project_key_mut(&key) {
                                    t.writer_state = WriterState::Active;
                                }
                                Ok(())
                            });
                            let _ = db.set_handoff_state(&key, "idle", None, None, None);
                            let msg = format!(
                                "引き渡しタグの公開後に公開済みスナップショットを確認できなかったため中止しました（staging={staging_id}）"
                            );
                            let _ = db.record_backup_run(
                                &started_at,
                                actor,
                                &target.label,
                                false,
                                Err(&msg),
                            );
                            let _ = db.record_audit(
                                actor,
                                "release_project_failed",
                                &format!("project={key} {msg}"),
                            );
                            self.set_status(&msg);
                            return Err(AppError::Restic(msg));
                        }
                        Ok(PromoteHandoffOutcome::LookupUncertain(lookup_err)) => {
                            // Keep Standby + `"releasing"` so re-running release can confirm or finish
                            // safely without risking an Active overwrite.
                            let msg = format!(
                                "引き渡しタグ公開後の確認が中断・失敗しました（重複書き込み防止のため待機中・引き渡し中断状態を維持します。「引き渡しを再開」で状態を確認・完了してください）: {lookup_err}"
                            );
                            let _ = db.record_audit(
                                actor,
                                "release_project_uncertain",
                                &format!("project={key} staging={staging_id} {lookup_err}"),
                            );
                            self.set_status(&msg);
                            return Err(AppError::Restic(msg));
                        }
                        Err(tag_err) => {
                            // Keep Standby + `"releasing"` instead of rolling back Active in case
                            // `restic tag` committed right before cancellation/signal.
                            let msg = format!(
                                "引き渡しタグの公開（restic tag）が中断または失敗しました（再実行で安全に復旧できます）: {tag_err}"
                            );
                            let _ = db.record_backup_run(
                                &started_at,
                                actor,
                                &target.label,
                                false,
                                Err(&msg),
                            );
                            let _ = db.record_audit(
                                actor,
                                "release_project_failed",
                                &format!("project={key} {msg}"),
                            );
                            self.set_status(&msg);
                            return Err(AppError::Restic(msg));
                        }
                    };
                    let _ = db.set_handoff_state(&key, "idle", None, None, None);
                    writer_state = WriterState::Standby;
                    summary.snapshot_id = Some(promoted_id.clone());
                    // Only clean up any superseded prior staging snapshot AFTER the new release is
                    // confirmed published!
                    if let Some(ref old_staging) = prev_staging_to_cleanup {
                        let _ = runner::forget_snapshot(&cfg, &secrets, old_staging);
                    }

                    let now_iso = Utc::now().to_rfc3339();
                    let _ = db.update_local_backup_sync(
                        &key,
                        &promoted_id,
                        &now_iso,
                        &local_machine,
                        true,
                        &target.path,
                        summary.exclude_fingerprint.as_deref(),
                    );
                    let _ = db.set_last_remote_checked_at(&now_iso);
                }

                let run_id = db.record_backup_run(
                    &started_at,
                    actor,
                    &target.label,
                    dry_run,
                    Ok(&(summary.clone(), elapsed_ms)),
                )?;

                let detail = format!(
                    "project={key} dry_run={dry_run} snapshot={:?} writerState={}",
                    summary.snapshot_id,
                    writer_state.as_str()
                );
                let _ = db.record_audit(actor, "release_project", &detail);
                self.set_status(if dry_run {
                    format!("引き渡しテスト完了: {}", target.label)
                } else {
                    format!("「{}」を引き渡し済みにしました（待機中）", target.label)
                });

                Ok(json!({
                    "projectKey": key,
                    "label": target.label,
                    "dryRun": dry_run,
                    "runId": run_id,
                    "snapshotId": summary.snapshot_id,
                    "writerState": writer_state,
                    "summary": summary,
                    "durationMs": elapsed_ms,
                }))
            }
            Err(err) => {
                let msg = err.to_string();
                if err.is_lock_timeout() {
                    let _ =
                        db.record_backup_deferred(&started_at, actor, &target.label, dry_run, &msg);
                } else {
                    let _ =
                        db.record_backup_run(&started_at, actor, &target.label, dry_run, Err(&msg));
                }
                let _ = db.record_audit(
                    actor,
                    "release_project_failed",
                    &format!("project={key} {msg}"),
                );
                self.set_status(format!("引き渡しに失敗しました: {msg}"));
                Err(err)
            }
        }
    }

    /// Takes over a shared project on this PC (6-stage flow; see `docs/ARCHITECTURE.md`).
    pub fn takeover_project(
        &self,
        actor: &str,
        params: TakeoverParams,
    ) -> AppResult<TakeoverReport> {
        let key = validate_project_key(&params.project_key)?;
        let cfg = self.get_config();
        if actor == "mcp" && !cfg.agent.allow_handoff {
            return Err(AppError::InvalidArgument(
                "MCP takeover_project is disabled by agent.allowHandoff policy in config.json"
                    .to_owned(),
            ));
        }
        if actor == "mcp" && !params.dry_run {
            // Exact match without trimming so padded/malformed strings cannot satisfy confirmation.
            let confirmed = params.confirm_project_key.as_deref() == Some(key.as_str());
            if !confirmed {
                return Err(AppError::InvalidArgument(format!(
                    "実引き継ぎを実行するには confirmProjectKey に \"{key}\" を正確に指定してください"
                )));
            }
        }

        let Ok(_job_lock) = self.job_mutex.try_lock() else {
            return Err(AppError::Restic(
                "another backup or maintenance operation is already running".to_owned(),
            ));
        };
        // Reset cancellation ONCE immediately after acquiring the job lock so user cancel
        // requests during Stage 1/3/4/5/6 are never wiped out mid-flight.
        self.cancel_flag.store(false, Ordering::Relaxed);

        let secrets = self.load_secrets()?;
        let db = HistoryDb::open(&self.db_path)?;
        let local_machine = cfg.effective_machine_name();
        let initial_storage_key = storage_destination_key(&cfg);
        let existing_target = cfg.find_target_by_project_key(&key).cloned();
        let local_folder_hint = existing_target
            .as_ref()
            .and_then(|t| snapshot_path_folder_name(&t.path).map(str::to_owned))
            .or_else(|| {
                params
                    .local_path
                    .as_deref()
                    .and_then(snapshot_path_folder_name)
                    .map(str::to_owned)
            });

        // Stage 1: Inspect latest effective snapshot in the repository (ignoring safety/staging).
        self.set_status(format!(
            "リポジトリの最新スナップショットを確認中 ({key}) ..."
        ));
        let all_snaps = runner::list_snapshots_filtered_cancellable(
            &cfg,
            &secrets,
            None,
            None,
            None,
            Some(RETRY_LOCK_MANUAL_OR_HANDOFF),
            &self.cancel_flag,
        )?;
        let checked_at = Utc::now().to_rfc3339();
        let configured_keys: Vec<String> = cfg
            .targets
            .iter()
            .filter_map(|t| t.project_key.clone())
            .collect();
        let _ = sync::ingest_snapshots_into_db(
            &db,
            &local_machine,
            &all_snaps,
            &checked_at,
            &configured_keys,
        );

        let initial_project_snap_ids: std::collections::BTreeSet<String> = all_snaps
            .iter()
            .filter(|s| {
                s.project_key() == Some(key.as_str())
                    && !s.is_safety_backup()
                    && !s.is_handoff_staging()
            })
            .map(|s| s.id.clone())
            .collect();

        let (source_snap, source_path, is_legacy_composite) = if let Some(snap) =
            sync::select_latest_effective_snapshot(&all_snaps, &key)
        {
            let matched = sync::resolve_snapshot_source_path(
                snap,
                &key,
                local_folder_hint.as_deref(),
            )
            .ok_or_else(|| {
                AppError::Restic(format!(
                    "スナップショット {} からプロジェクト \"{key}\" の元パスを特定できませんでした",
                    snap.display_short_id()
                ))
            })?;
            (snap.clone(), matched.to_owned(), false)
        } else if let Some((snap, matched)) =
            sync::select_legacy_composite_candidate(&all_snaps, &key, local_folder_hint.as_deref())?
        {
            (snap.clone(), matched.to_owned(), true)
        } else {
            return Err(AppError::InvalidArgument(format!(
                "リポジトリにプロジェクト \"{key}\" の有効なスナップショットが見つかりません"
            )));
        };

        // Even if another PC's system clock was skewed behind `source_snap.time`, its claim
        // carries `handoff:from:<source_snap.id>`. Refuse dual takeover of an already-claimed
        // snapshot unless `dry_run` preview (where we surface it via Err/warning).
        if let Some(already_claimed) = all_snaps.iter().find(|s| {
            s.project_key() == Some(key.as_str())
                && !s.is_safety_backup()
                && !s.is_handoff_staging()
                && !s.normalized_host().eq_ignore_ascii_case(&local_machine)
                && s.handoff_from_snapshot_id() == Some(source_snap.id.as_str())
        }) {
            return Err(AppError::InvalidArgument(format!(
                "スナップショット {} は既に他 PC（{}）によって引き継ぎ済み（{}）のため、二重取得を防止するため中止しました（PC 間の時計ずれがないか確認してください）",
                source_snap.display_short_id(),
                already_claimed.normalized_host(),
                already_claimed.display_short_id()
            )));
        }

        if let Some(ref expected_id) = params.expected_source_snapshot_id
            && !expected_id.trim().is_empty()
            && expected_id != &source_snap.id
        {
            return Err(AppError::InvalidArgument(format!(
                "確認後にリポジトリの最新スナップショットが更新されました（確認時 {} → 最新 {}）。内容を再確認してから引き継ぎを実行してください",
                &expected_id[..expected_id.len().min(8)],
                source_snap.display_short_id()
            )));
        }

        let source_subpath = windows_path_to_restic_subpath(&source_path)?;
        let source_host = source_snap.normalized_host();
        let remote_released = source_snap.is_handoff_released();
        let remote_possibly_active =
            !remote_released && !source_host.eq_ignore_ascii_case(&local_machine);

        // For untagged legacy snapshots, verify BEFORE touching the destination that the selected
        // subtree does not contain any `.git` repository (at root or nested).
        if is_legacy_composite
            && runner::snapshot_subpath_has_git(
                &cfg,
                &secrets,
                &source_snap.id,
                &source_subpath,
                &self.cancel_flag,
            )?
        {
            return Err(AppError::InvalidArgument(format!(
                "復元元スナップショット（{}）の対象サブツリーに .git が含まれるため、Restic 引き継ぎの対象外です（GitHub 経由で同期してください）",
                source_snap.display_short_id()
            )));
        }

        // Stage 2: Resolve and validate local destination directory.
        let requested_local_path = match (
            params
                .local_path
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty()),
            &existing_target,
        ) {
            (Some(p), _) => PathBuf::from(p),
            (None, Some(t)) => PathBuf::from(&t.path),
            (None, None) => sync::suggest_takeover_destination(&cfg, &source_path, &key)
                .ok_or_else(|| {
                    AppError::InvalidArgument(
                        "この PC に未配置のプロジェクトです。復元先フォルダ（localPath）を指定するか、設定で既定のプロジェクト親フォルダ（defaultProjectRoot）を設定してください"
                            .to_owned(),
                    )
                })?,
        };
        let safe_dest = validate_restore_destination(&requested_local_path)?;
        let safe_dest_str = safe_dest.to_string_lossy().into_owned();
        if let Some(ref expected_canon) = params.expected_canonical_path
            && !expected_canon.trim().is_empty()
            && safe_dest_str != *expected_canon
        {
            return Err(AppError::InvalidArgument(format!(
                "確認後に復元先フォルダの実体パスが変化しました（確認時 {expected_canon} → 現在 {safe_dest_str}）。内容を再確認してから引き継ぎを実行してください"
            )));
        }
        // Fail closed on metadata errors instead of `Path::exists()` swallowing errors as `false`.
        let dest_meta = match std::fs::metadata(&safe_dest) {
            Ok(m) => Some(m),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => {
                return Err(AppError::Restic(format!(
                    "復元先フォルダの状態確認に失敗したため引き継ぎを中止しました（{}）: {e}",
                    safe_dest.display()
                )));
            }
        };
        let dest_exists = dest_meta.is_some();
        if let Some(ref m) = dest_meta
            && !m.is_dir()
        {
            return Err(AppError::InvalidArgument(format!(
                "復元先パスはディレクトリではありません: {}",
                safe_dest.display()
            )));
        }
        // Reject destination overlapping any OTHER registered target BEFORE modifying disk.
        for other in &cfg.targets {
            if other.project_key.as_deref() == Some(key.as_str()) {
                continue;
            }
            if paths_overlap(&safe_dest, Path::new(&other.path)) {
                return Err(AppError::InvalidArgument(format!(
                    "復元先フォルダ（{}）が別の登録済み対象「{}」（{}）と重複または包含関係にあるため中止しました",
                    safe_dest.display(),
                    other.label,
                    other.path
                )));
            }
        }
        if is_git_repository(&safe_dest) {
            return Err(AppError::InvalidArgument(format!(
                "復元先フォルダは Git 管理リポジトリ（.git 配下を含む）のため Restic 引き継ぎの対象外です。GitHub 経由で同期してください ({})",
                safe_dest.display()
            )));
        }
        if dest_exists && let Some(lockfile) = find_unity_lockfile(&safe_dest) {
            return Err(AppError::InvalidArgument(format!(
                "復元先フォルダを Unity エディタが開いているため引き継ぎを中止しました（{} を検出）。Unity を閉じてからやり直してください",
                lockfile.display()
            )));
        }

        let same_local_path = existing_target.as_ref().is_some_and(|t| {
            let existing_canon =
                dunce::canonicalize(&t.path).unwrap_or_else(|_| PathBuf::from(&t.path));
            existing_canon
                .to_string_lossy()
                .eq_ignore_ascii_case(&safe_dest_str)
        });

        // Always build probe/safety-backup target from `safe_dest` (NEVER stale registered path!).
        let target_excludes = if same_local_path {
            existing_target
                .as_ref()
                .map(|t| t.exclude_patterns.clone())
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        let mut dest_target = BackupTarget::new(
            existing_target
                .as_ref()
                .map(|t| t.label.clone())
                .unwrap_or_else(|| {
                    snapshot_path_folder_name(&source_path)
                        .unwrap_or(&key)
                        .to_owned()
                }),
            &safe_dest,
        );
        dest_target.project_key = Some(key.clone());
        dest_target.exclude_patterns = target_excludes.clone();
        if same_local_path && let Some(ref existing) = existing_target {
            dest_target.tags = existing.tags.clone();
        }

        // Stage 3: Detect unbacked local changes and exact interrupted-handoff resume eligibility.
        // Require exact canonical path match (not case-insensitive) so case-sensitive Windows
        // directories (`World` vs `world`) can never hijack an interrupted `claiming` checkpoint.
        let existing_sync_row = db.get_project_sync_state(&key)?;
        let prev_safety_id = existing_sync_row
            .as_ref()
            .and_then(|r| r.handoff_safety_snapshot_id.clone());
        let interrupted_same_source_and_target = existing_sync_row.as_ref().is_some_and(|r| {
            r.handoff_status != "idle"
                && r.handoff_target_path.as_deref() == Some(safe_dest_str.as_str())
                && r.handoff_source_snapshot_id.as_deref() == Some(source_snap.id.as_str())
        });
        let resume_claiming = interrupted_same_source_and_target
            && existing_sync_row
                .as_ref()
                .is_some_and(|r| r.handoff_status == "claiming");
        let resume_restoring = interrupted_same_source_and_target
            && existing_sync_row
                .as_ref()
                .is_some_and(|r| r.handoff_status == "restoring");
        let was_interrupted = resume_claiming || resume_restoring;

        // Fail closed if `read_dir(&safe_dest)` errors so transient I/O errors can never be
        // mistaken for an empty folder and skip safety backups.
        let dest_non_empty = if dest_exists {
            let mut entries = std::fs::read_dir(&safe_dest).map_err(|err| {
                AppError::Restic(format!(
                    "復元先フォルダの中身を確認できなかったため引き継ぎを中止しました（{}）: {err}",
                    safe_dest.display()
                ))
            })?;
            match entries.next() {
                Some(Ok(_)) => true,
                Some(Err(err)) => {
                    return Err(AppError::Restic(format!(
                        "復元先フォルダの読み取りに失敗したため引き継ぎを中止しました（{}）: {err}",
                        safe_dest.display()
                    )));
                }
                None => false,
            }
        } else {
            false
        };

        let (has_unbacked_local_changes, unbacked_changes_reason) = if !dest_non_empty {
            (false, None)
        } else if resume_claiming {
            (
                false,
                Some(
                    "前回の引き継ぎ復元は完了済みで所有権スナップショット保存のみ未完了です。再復元せず所有権登録から再開します"
                        .to_owned(),
                ),
            )
        } else if resume_restoring {
            // Even when resuming an interrupted restore, treat non-empty destination as potentially
            // containing post-interruption edits so safety backup / discard confirmation still applies.
            (
                true,
                Some(
                    "前回の引き継ぎ復元が途中で中断したフォルダです。中断後の編集を保護するため退避バックアップ保存（または明示破棄確認）のうえで再復元します"
                        .to_owned(),
                ),
            )
        } else if !same_local_path {
            (
                true,
                Some(
                    "登録済みパスとは異なる既存フォルダが指定されました（未管理ファイルを含む可能性があります）"
                        .to_owned(),
                ),
            )
        } else if existing_sync_row
            .as_ref()
            .and_then(|r| r.last_synced_snapshot_id.as_ref())
            .is_none()
        {
            (
                true,
                Some(
                    "この PC での同期記録がない既存フォルダです（未管理の変更を含む可能性があります）"
                        .to_owned(),
                ),
            )
        } else {
            let parent = existing_sync_row
                .as_ref()
                .and_then(|r| r.last_local_backup_snapshot_id.as_deref());
            match runner::check_target_local_changes_cancellable(
                &cfg,
                &secrets,
                &dest_target,
                parent,
                &self.cancel_flag,
            ) {
                Ok(true) => (
                    true,
                    Some("前回同期以降にローカルフォルダへ追加・変更・削除されたファイルがあります".to_owned()),
                ),
                Ok(false) => (false, None),
                Err(_) if self.cancel_flag.load(Ordering::Relaxed) => {
                    return Err(AppError::Restic(
                        "takeover cancelled by user or agent".to_owned(),
                    ));
                }
                Err(_) => (
                    true,
                    Some("ローカル変更の事前比較が完了できなかったため安全側（変更あり）として扱います".to_owned()),
                ),
            }
        };

        let local_fp = exclude_fingerprint(&cfg, std::slice::from_ref(&dest_target));
        let remote_fp = source_snap.exclude_fingerprint().map(str::to_owned);
        let fp_mismatch = matches!(remote_fp.as_deref(), Some(rfp) if rfp != local_fp);

        let mut warnings = Vec::new();
        if is_legacy_composite {
            warnings.push(
                "旧形式の複合スナップショットから対象サブフォルダのみを抽出して復元します"
                    .to_owned(),
            );
        }
        if remote_possibly_active {
            warnings.push(format!(
                "相手 PC（{source_host}）が引き渡し操作を完了していないため作業中の可能性があります。相手側の未バックアップ変更は引き継がれません"
            ));
        }
        if has_unbacked_local_changes && let Some(ref reason) = unbacked_changes_reason {
            warnings.push(reason.clone());
        }
        if resume_claiming {
            warnings.push(
                "復元は前回完了済みのためローカルファイルを上書きせず、所有権スナップショット保存から再開します"
                    .to_owned(),
            );
        } else if resume_restoring {
            warnings.push(
                "前回の引き継ぎ復元が途中で中断しています。再実行して最新の状態へ収束させます"
                    .to_owned(),
            );
        }
        if fp_mismatch {
            warnings.push(
                "相手 PC とこの PC で除外設定（excludePatterns 等）が異なります。引き継ぎ後に除外設定を見直してください"
                    .to_owned(),
            );
        }

        let will_save_safety_backup =
            has_unbacked_local_changes && !resume_claiming && params.save_local_backup_first;
        let mut report = TakeoverReport {
            dry_run: params.dry_run,
            project_key: key.clone(),
            source_snapshot_id: source_snap.id.clone(),
            source_short_id: source_snap.display_short_id().to_owned(),
            source_host: source_host.clone(),
            source_time: source_snap.time.clone(),
            source_path: source_path.clone(),
            source_subpath: source_subpath.clone(),
            is_legacy_composite,
            remote_released,
            remote_possibly_active,
            local_path: safe_dest_str.clone(),
            local_dir_exists: dest_exists,
            has_unbacked_local_changes,
            unbacked_changes_reason,
            was_interrupted,
            exclude_fingerprint_mismatch: fp_mismatch,
            local_exclude_fingerprint: Some(local_fp),
            remote_exclude_fingerprint: remote_fp,
            delete_extra: params.delete_extra,
            will_save_safety_backup,
            safety_backup_snapshot_id: prev_safety_id.clone(),
            claimed_snapshot_id: None,
            warnings,
        };

        if params.dry_run {
            let _ = db.record_audit(
                actor,
                "takeover_project_preview",
                &format!(
                    "project={key} source={} host={source_host} localChanges={has_unbacked_local_changes} remoteActive={remote_possibly_active}",
                    report.source_short_id
                ),
            );
            self.set_status(format!("引き継ぎ内容を確認しました ({key})"));
            return Ok(report);
        }

        if self.cancel_flag.load(Ordering::Relaxed) {
            return Err(AppError::Restic(
                "takeover cancelled by user or agent".to_owned(),
            ));
        }

        // Stage 4: Guard against overwriting remote-active or unbacked local changes without confirmation.
        if remote_possibly_active && !params.force_while_remote_active {
            let msg = format!(
                "相手 PC（{source_host}）の最新スナップショットは引き渡し済み（handoff:released）ではありません。作業中の可能性があります。強制引き継ぎする場合は forceWhileRemoteActive: true を指定してください"
            );
            let _ = db.record_audit(actor, "takeover_project_failed", &msg);
            return Err(AppError::InvalidArgument(msg));
        }
        if has_unbacked_local_changes
            && !resume_claiming
            && !params.save_local_backup_first
            && !params.discard_local_changes
        {
            let msg = "受け側フォルダに未バックアップの変更があります。退避スナップショットを保存してから引き継ぐか、破棄を明示確認（confirmDiscardLocalChanges: true）してください".to_owned();
            let _ = db.record_audit(actor, "takeover_project_failed", &msg);
            return Err(AppError::InvalidArgument(msg));
        }

        let label = dest_target.label.clone();

        if !resume_claiming {
            // Optional Stage 4b: Save pre-takeover safety backup (`handoff:safety-backup`) of `safe_dest`.
            if will_save_safety_backup {
                if self.cancel_flag.load(Ordering::Relaxed) {
                    return Err(AppError::Restic(
                        "takeover cancelled by user or agent".to_owned(),
                    ));
                }
                self.set_status(format!(
                    "現在のローカル内容を退避バックアップ中 ({label}) ..."
                ));
                let safety_started = Utc::now().to_rfc3339();
                let safety_tags = vec![TAG_HANDOFF_SAFETY_BACKUP.to_owned()];
                let opts = BackupTargetOptions {
                    dry_run: false,
                    extra_tags: &[],
                    internal_tags: &safety_tags,
                    suppress_project_tag: false,
                    retry_lock: RETRY_LOCK_MANUAL_OR_HANDOFF,
                    parent_snapshot: None,
                    progress_label: Some(&format!("退避: {label}")),
                };
                match runner::run_backup_target(
                    &cfg,
                    &secrets,
                    &dest_target,
                    &opts,
                    &self.progress,
                    &self.cancel_flag,
                ) {
                    Ok((summary, elapsed_ms)) => {
                        let _ = db.record_backup_run(
                            &safety_started,
                            actor,
                            &format!("{label} (退避)"),
                            false,
                            Ok(&(summary.clone(), elapsed_ms)),
                        );
                        // Safety backup MUST exit cleanly (code 0) with a real snapshot ID before
                        // restore is allowed to proceed.
                        if summary.partial_read_warning || summary.snapshot_id.is_none() {
                            let msg = format!(
                                "退避スナップショット作成時に読み取れないファイルがあったため（終了コード 3）、データ保護のため復元を中止しました: {}",
                                summary.unreadable.join("; ")
                            );
                            let _ = db.record_audit(actor, "takeover_project_failed", &msg);
                            self.set_status(&msg);
                            return Err(AppError::Restic(msg));
                        }
                        let _ = db.record_audit(
                            actor,
                            "takeover_safety_backup",
                            &format!("project={key} snapshot={:?}", summary.snapshot_id),
                        );
                        report.safety_backup_snapshot_id = summary.snapshot_id;
                    }
                    Err(err) => {
                        let msg = format!(
                            "退避スナップショットの保存に失敗したため引き継ぎを中止しました: {err}"
                        );
                        let _ = db.record_audit(actor, "takeover_project_failed", &msg);
                        self.set_status(&msg);
                        return Err(AppError::Restic(msg));
                    }
                }
            }

            if self.cancel_flag.load(Ordering::Relaxed) {
                return Err(AppError::Restic(
                    "takeover cancelled by user or agent".to_owned(),
                ));
            }

            // Always build restore exclude patterns (even when `delete_extra == false`) so
            // receiver-excluded files are neither deleted nor overwritten by the sender's snapshot.
            let exclude_patterns = restore_delete_exclude_patterns(
                &cfg,
                &safe_dest,
                &target_excludes,
                Some(&self.cancel_flag),
            )?;

            // Re-check cancellation after directory scan and immediately before persisting
            // `"restoring"` so cancelling before restore starts never leaves a spurious interrupted state.
            if self.cancel_flag.load(Ordering::Relaxed) {
                return Err(AppError::Restic(
                    "takeover cancelled by user or agent".to_owned(),
                ));
            }

            // Stage 5: Subpath restore with `--exclude` protection (and optional `--delete`).
            db.set_handoff_state(
                &key,
                "restoring",
                Some(&source_snap.id),
                Some(&safe_dest_str),
                report.safety_backup_snapshot_id.as_deref(),
            )?;
            self.set_status(format!("引き継ぎデータを復元中 ({label}) ..."));
            let restore_opts = RestoreSubpathOptions {
                snapshot_id: &source_snap.id,
                subpath: &source_subpath,
                target_dir: &safe_dest,
                delete_extra: params.delete_extra,
                exclude_patterns: &exclude_patterns,
                retry_lock: RETRY_LOCK_MANUAL_OR_HANDOFF,
                progress_label: &label,
            };
            let restore_spawned = AtomicBool::new(false);
            if let Err(err) = runner::restore_subpath(
                &cfg,
                &secrets,
                &restore_opts,
                &self.progress,
                &self.cancel_flag,
                &restore_spawned,
            ) {
                // If `restic restore` never spawned (e.g. cancelled or destination changed before
                // spawn), revert `handoff_status` back to its prior state so an untouched folder is
                // not falsely stuck in `"restoring"`.
                if !restore_spawned.load(Ordering::Relaxed) {
                    if let Some(ref prev) = existing_sync_row {
                        let _ = db.set_handoff_state(
                            &key,
                            &prev.handoff_status,
                            prev.handoff_source_snapshot_id.as_deref(),
                            prev.handoff_target_path.as_deref(),
                            report
                                .safety_backup_snapshot_id
                                .as_deref()
                                .or(prev.handoff_safety_snapshot_id.as_deref()),
                        );
                    } else {
                        let _ = db.set_handoff_state(
                            &key,
                            "idle",
                            None,
                            None,
                            report.safety_backup_snapshot_id.as_deref(),
                        );
                    }
                }
                let msg = format!("引き継ぎ復元が中断または失敗しました: {err}");
                let _ = db.record_audit(
                    actor,
                    "takeover_project_failed",
                    &format!("project={key} {msg}"),
                );
                self.set_status(&msg);
                return Err(err);
            }
        }

        // Stage 6: Re-verify no other PC claimed this project while we were restoring, verify
        // storage config did not change mid-flight, register/activate target, and atomically
        // stage + promote `handoff:claimed`. Transient lookup failures must not erase the
        // saved resume checkpoint or safety snapshot ID.
        match verify_no_concurrent_remote_claim(
            &cfg,
            &secrets,
            &key,
            &local_machine,
            &source_snap.id,
            &initial_project_snap_ids,
            &self.cancel_flag,
        )? {
            ConcurrentClaimCheck::Clear => {}
            ConcurrentClaimCheck::ClaimedByOther(msg) => {
                let _ = db.set_handoff_state(&key, "idle", None, None, None);
                let _ = db.record_audit(actor, "takeover_project_failed", &msg);
                self.set_status(&msg);
                return Err(AppError::InvalidArgument(msg));
            }
        }

        let canonical_dest = canonicalize_existing_dir(&safe_dest, "restored project directory")?;
        let canonical_str = canonical_dest.to_string_lossy().into_owned();
        let mut registered = None;
        let expected_storage = initial_storage_key.clone();
        self.update_config(|c| {
            if storage_destination_key(c) != expected_storage {
                return Err(AppError::InvalidArgument(
                    "引き継ぎ実行中に保存先リポジトリ設定が変更されたため所有権登録を中止しました"
                        .to_owned(),
                ));
            }
            let t = c.register_shared_target(
                &canonical_dest,
                Some(label.clone()),
                &key,
                WriterState::Active,
            )?;
            registered = Some(t);
            Ok(())
        })?;
        let registered_target = registered.expect("register_shared_target ran");

        db.set_handoff_state(
            &key,
            "claiming",
            Some(&source_snap.id),
            Some(&canonical_str),
            report.safety_backup_snapshot_id.as_deref(),
        )?;
        if self.cancel_flag.load(Ordering::Relaxed) {
            return Err(AppError::Restic(
                "takeover cancelled before ownership claim snapshot".to_owned(),
            ));
        }

        self.set_status(format!("所有権スナップショットを保存中 ({label}) ..."));
        let claim_started = Utc::now().to_rfc3339();
        let staging_tags = vec![TAG_HANDOFF_STAGING.to_owned()];
        let claim_opts = BackupTargetOptions {
            dry_run: false,
            extra_tags: &[],
            internal_tags: &staging_tags,
            suppress_project_tag: true,
            retry_lock: RETRY_LOCK_MANUAL_OR_HANDOFF,
            parent_snapshot: None,
            progress_label: Some(&format!("所有権登録: {label}")),
        };
        // Always use the pinned `&cfg` (same repository we restored from) for staging & tag promotion.
        match runner::run_backup_target(
            &cfg,
            &secrets,
            &registered_target,
            &claim_opts,
            &self.progress,
            &self.cancel_flag,
        ) {
            Ok((mut summary, elapsed_ms)) => {
                if summary.partial_read_warning || summary.snapshot_id.is_none() {
                    let cleanup_note = summary
                        .snapshot_id
                        .as_deref()
                        .map(|id| cleanup_staging_snapshot_note(&cfg, &secrets, id))
                        .unwrap_or_default();
                    let msg = format!(
                        "所有権スナップショット作成時に読み取れないファイルがあったため（終了コード 3）、不完全なスナップショットを公開せず中断しました（再実行で所有権登録から再開できます）: {}{cleanup_note}",
                        summary.unreadable.join("; ")
                    );
                    let _ = db.record_backup_run(
                        &claim_started,
                        actor,
                        &registered_target.label,
                        false,
                        Err(&msg),
                    );
                    let _ = db.record_audit(actor, "takeover_project_failed", &msg);
                    self.set_status(&msg);
                    return Err(AppError::Restic(msg));
                }

                let staging_id = summary
                    .snapshot_id
                    .clone()
                    .ok_or_else(|| AppError::Restic("missing claim staging id".to_owned()))?;

                // Pre-tag remote check (preserves `"claiming"` checkpoint on transient errors).
                match verify_no_concurrent_remote_claim(
                    &cfg,
                    &secrets,
                    &key,
                    &local_machine,
                    &source_snap.id,
                    &initial_project_snap_ids,
                    &self.cancel_flag,
                ) {
                    Ok(ConcurrentClaimCheck::Clear) => {}
                    Ok(ConcurrentClaimCheck::ClaimedByOther(msg)) => {
                        let note = cleanup_staging_snapshot_note(&cfg, &secrets, &staging_id);
                        let key_clone = key.clone();
                        let _ = self.update_config(|c| {
                            if let Some(t) = c.find_target_by_project_key_mut(&key_clone) {
                                t.writer_state = WriterState::Standby;
                            }
                            Ok(())
                        });
                        let _ = db.set_handoff_state(&key, "idle", None, None, None);
                        let full = format!("{msg}{note}");
                        let _ = db.record_audit(actor, "takeover_project_failed", &full);
                        self.set_status(&full);
                        return Err(AppError::InvalidArgument(full));
                    }
                    Err(err) => {
                        let note = cleanup_staging_snapshot_note(&cfg, &secrets, &staging_id);
                        let full = format!(
                            "所有権公開前のリモート確認に失敗しました（再実行で所有権登録から再開できます）: {err}{note}"
                        );
                        let _ = db.record_audit(actor, "takeover_project_failed", &full);
                        self.set_status(&full);
                        return Err(AppError::Restic(full));
                    }
                }

                let from_tags = vec![format!("{TAG_HANDOFF_FROM_PREFIX}{}", source_snap.id)];
                let (promoted_id, post_tag_snaps) = match runner::promote_handoff_snapshot(
                    &cfg,
                    &secrets,
                    &staging_id,
                    &key,
                    TAG_HANDOFF_CLAIMED,
                    &from_tags,
                    RETRY_LOCK_MANUAL_OR_HANDOFF,
                    &self.cancel_flag,
                ) {
                    Ok(PromoteHandoffOutcome::Confirmed {
                        snapshot_id,
                        project_snapshots,
                    }) => (snapshot_id, project_snapshots),
                    Ok(PromoteHandoffOutcome::NotFoundAfterLookup) => {
                        let msg = format!(
                            "所有権スナップショットの公開後に公開済み ID を確認できませんでした（再実行で所有権登録から再開できます・staging={staging_id}）"
                        );
                        let _ = db.record_audit(actor, "takeover_project_failed", &msg);
                        self.set_status(&msg);
                        return Err(AppError::Restic(msg));
                    }
                    Ok(PromoteHandoffOutcome::LookupUncertain(detail)) => {
                        let msg = format!(
                            "所有権スナップショット公開後の確認に失敗しました（再実行または最新状態確認を行ってください）: {detail}"
                        );
                        let _ = db.record_audit(actor, "takeover_project_failed", &msg);
                        self.set_status(&msg);
                        return Err(AppError::Restic(msg));
                    }
                    Err(tag_err) => {
                        let msg = format!(
                            "所有権スナップショットの公開（restic tag）に失敗しました（再実行で所有権登録から再開できます）: {tag_err}"
                        );
                        let _ = db.record_backup_run(
                            &claim_started,
                            actor,
                            &registered_target.label,
                            false,
                            Err(&msg),
                        );
                        let _ = db.record_audit(actor, "takeover_project_failed", &msg);
                        self.set_status(&msg);
                        return Err(AppError::Restic(msg));
                    }
                };

                // Linearizable post-tag verification: if another PC concurrently published a
                // claim, yield by reverting THIS PC to `Standby` and returning `Err` WITHOUT
                // deleting `promoted_id` (so a concurrent maintenance `forget_prune` planning to
                // keep `promoted_id` can never end up deleting all remaining snapshots!).
                if let ConcurrentClaimCheck::ClaimedByOther(conflict_msg) =
                    find_concurrent_remote_claim_in_snapshots(
                        &post_tag_snaps,
                        &key,
                        &local_machine,
                        &source_snap.id,
                        &initial_project_snap_ids,
                        Some(&promoted_id),
                    )
                {
                    let key_clone = key.clone();
                    let _ = self.update_config(|c| {
                        if let Some(t) = c.find_target_by_project_key_mut(&key_clone) {
                            t.writer_state = WriterState::Standby;
                        }
                        Ok(())
                    });
                    let _ = db.set_handoff_state(&key, "idle", None, None, None);
                    let _ = db.record_audit(actor, "takeover_project_conflict", &conflict_msg);
                    self.set_status(&conflict_msg);
                    return Err(AppError::InvalidArgument(conflict_msg));
                }

                summary.snapshot_id = Some(promoted_id.clone());

                let _ = db.record_backup_run(
                    &claim_started,
                    actor,
                    &registered_target.label,
                    false,
                    Ok(&(summary.clone(), elapsed_ms)),
                );
                let now_iso = Utc::now().to_rfc3339();
                db.update_local_backup_sync(
                    &key,
                    &promoted_id,
                    &now_iso,
                    &local_machine,
                    false,
                    &registered_target.path,
                    summary.exclude_fingerprint.as_deref(),
                )?;
                let _ = db.set_last_remote_checked_at(&now_iso);

                report.claimed_snapshot_id = Some(promoted_id);
                report.local_path = registered_target.path.clone();
                let _ = db.record_audit(
                    actor,
                    "takeover_project",
                    &format!(
                        "project={key} from={} claimed={:?} safety={:?} delete={}",
                        report.source_short_id,
                        report.claimed_snapshot_id,
                        report.safety_backup_snapshot_id,
                        params.delete_extra
                    ),
                );
                self.set_status(format!(
                    "「{}」をこの PC で引き継ぎました",
                    registered_target.label
                ));
                Ok(report)
            }
            Err(err) => {
                let msg = format!(
                    "復元は完了しましたが所有権スナップショットの作成に失敗しました（引き継ぎを再実行してください）: {err}"
                );
                let _ = db.record_audit(
                    actor,
                    "takeover_project_failed",
                    &format!("project={key} {msg}"),
                );
                self.set_status(&msg);
                Err(AppError::Restic(msg))
            }
        }
    }

    /// Folder sizes in a snapshot, largest first, grouped under the current backup folders.
    pub fn snapshot_folder_sizes(&self, snapshot_id: &str) -> AppResult<Vec<FolderBytes>> {
        let cfg = self.get_config();
        runner::snapshot_folder_sizes(&cfg, &self.load_secrets()?, snapshot_id, &cfg.targets)
    }

    /// `restic init`, recorded in the audit log under `actor` on both success and failure.
    pub fn init_repository(&self, actor: &str) -> AppResult<String> {
        let Ok(_job_lock) = self.job_mutex.try_lock() else {
            return Err(AppError::Restic(
                "another backup or maintenance operation is already running".to_owned(),
            ));
        };
        let cfg = self.get_config();
        self.set_status("R2 にリポジトリを作成中...");
        let result = self
            .load_secrets()
            .and_then(|secrets| runner::init_repository(&cfg, &secrets));
        match &result {
            Ok(out) => {
                self.record_audit(actor, "init_repository", out);
                self.set_status("R2 にリポジトリを作成しました");
            }
            Err(err) => {
                let msg = err.to_string();
                self.record_audit(actor, "init_repository_failed", &msg);
                self.set_status(format!("リポジトリ作成失敗: {msg}"));
            }
        }
        result
    }

    /// `restic check`, recorded in the audit log under `actor` on both success and failure.
    pub fn check_repository(&self, actor: &str) -> AppResult<String> {
        let Ok(_job_lock) = self.job_mutex.try_lock() else {
            return Err(AppError::Restic(
                "another backup or maintenance operation is already running".to_owned(),
            ));
        };
        let cfg = self.get_config();
        self.set_status("R2 のリポジトリ整合性を確認中...");
        let result = self
            .load_secrets()
            .and_then(|secrets| runner::check_repository(&cfg, &secrets));
        match &result {
            Ok(_) => {
                self.record_audit(actor, "check_repository", "restic check passed");
                self.set_status("接続と整合性を確認しました。問題はありません");
            }
            Err(err) => {
                let msg = err.to_string();
                self.record_audit(actor, "check_repository_failed", &msg);
                self.set_status(format!("整合性チェック失敗: {msg}"));
            }
        }
        result
    }

    /// Restore into a validated folder, recorded in the audit log under `actor`.
    pub fn restore_snapshot(
        &self,
        actor: &str,
        snapshot_id: &str,
        target_dir: &Path,
        include_paths: &[String],
    ) -> AppResult<String> {
        let msg = runner::restore_snapshot(
            &self.get_config(),
            &self.load_secrets()?,
            snapshot_id,
            target_dir,
            include_paths,
        )?;
        self.record_audit(actor, "restore_snapshot", &msg);
        Ok(msg)
    }

    /// Preview (`dry_run: true`) or execute (`dry_run: false`) deletion of objects under the
    /// configured R2 prefix or the entire R2 bucket using `config.r2` and DPAPI R2 keys.
    pub fn delete_r2_data(
        &self,
        actor: &str,
        scope: R2DeleteScope,
        confirm_bucket_name: Option<&str>,
        dry_run: bool,
    ) -> AppResult<R2DeleteReport> {
        let cfg = self.get_config();
        if actor == "mcp" && !cfg.agent.allow_delete_storage {
            return Err(AppError::InvalidArgument(
                "MCP R2 bucket/prefix deletion is disabled by agent.allowDeleteStorage policy in config.json"
                    .to_owned(),
            ));
        }

        if cfg.storage_backend != crate::config::StorageBackend::R2 {
            return Err(AppError::Config(
                "バケット削除は Cloudflare R2 用です。Rclone アダプタ使用中は rclone でデータを管理してください".to_owned(),
            ));
        }
        if !cfg.r2.is_configured() {
            return Err(AppError::Config(
                "Cloudflare R2 のアカウント ID とバケット名が設定されていません".to_owned(),
            ));
        }
        let bucket = cfg.r2.normalized_bucket();
        let prefix = cfg.r2.normalized_prefix();
        if scope == R2DeleteScope::PrefixOnly && prefix.is_empty() {
            return Err(AppError::InvalidArgument(
                "プレフィックスが設定されていないため「プレフィックス配下のみ削除」は実行できません"
                    .to_owned(),
            ));
        }
        if !dry_run {
            let confirmed = confirm_bucket_name
                .map(str::trim)
                .is_some_and(|v| v == bucket);
            if !confirmed {
                return Err(AppError::InvalidArgument(format!(
                    "実削除を実行するには confirmBucketName にバケット名 \"{bucket}\" を正確に指定してください"
                )));
            }
        }

        let Ok(_job_lock) = self.job_mutex.try_lock() else {
            return Err(AppError::Restic(
                "another backup or maintenance operation is already running".to_owned(),
            ));
        };

        let secrets = self.load_secrets()?;
        self.cancel_flag.store(false, Ordering::Relaxed);
        let status_label = match (dry_run, scope) {
            (true, R2DeleteScope::PrefixOnly) => {
                format!("R2 プレフィックス {bucket}/{prefix}/ の削除対象を確認中...")
            }
            (true, R2DeleteScope::EntireBucket) => {
                format!("R2 バケット「{bucket}」の削除対象を確認中...")
            }
            (false, R2DeleteScope::PrefixOnly) => {
                format!("R2 プレフィックス {bucket}/{prefix}/ を削除中...")
            }
            (false, R2DeleteScope::EntireBucket) => {
                format!("R2 バケット「{bucket}」を削除中...")
            }
        };
        self.set_status(status_label);

        let result = r2_s3::delete_r2_data(
            &cfg.r2,
            &secrets,
            scope,
            dry_run,
            &self.r2_delete_progress,
            &self.cancel_flag,
        );

        match &result {
            Ok(report) => {
                let target_desc = match &report.prefix {
                    Some(p) => format!("{}/{}", report.bucket, p),
                    None => report.bucket.clone(),
                };
                if dry_run {
                    let detail = format!(
                        "scope={} target={} objects={} bytes={} multipart={}",
                        scope.as_str(),
                        target_desc,
                        report.objects_found,
                        report.bytes_found,
                        report.multipart_uploads_found,
                    );
                    self.record_audit(actor, "delete_r2_preview", &detail);
                    self.set_status(format!(
                        "R2 削除対象を確認しました: {} 件 ({} B)",
                        report.objects_found, report.bytes_found
                    ));
                } else {
                    let (op, status) = match scope {
                        R2DeleteScope::PrefixOnly => (
                            "delete_r2_prefix",
                            format!(
                                "R2 プレフィックス {target_desc} を削除しました（{} 件）",
                                report.objects_deleted
                            ),
                        ),
                        R2DeleteScope::EntireBucket if report.bucket_deleted => (
                            "delete_r2_bucket",
                            format!(
                                "R2 バケット「{}」を削除しました（{} 件のオブジェクトを削除）",
                                report.bucket, report.objects_deleted
                            ),
                        ),
                        R2DeleteScope::EntireBucket if report.bucket_delete_access_denied => (
                            "delete_r2_bucket_emptied",
                            format!(
                                "R2 バケット「{}」を空にしました（{} 件削除・本体削除は 403 権限不足）",
                                report.bucket, report.objects_deleted
                            ),
                        ),
                        R2DeleteScope::EntireBucket => (
                            "delete_r2_bucket",
                            format!("R2 バケット「{}」はすでに存在しませんでした", report.bucket),
                        ),
                    };
                    let detail = format!(
                        "scope={} target={} deleted={} bytes={} multipartAborted={} bucketDeleted={} accessDenied={} notFound={}",
                        scope.as_str(),
                        target_desc,
                        report.objects_deleted,
                        report.bytes_deleted,
                        report.multipart_uploads_aborted,
                        report.bucket_deleted,
                        report.bucket_delete_access_denied,
                        report.bucket_not_found,
                    );
                    self.record_audit(actor, op, &detail);
                    self.set_status(status);
                }
            }
            Err(err) => {
                let msg = err.to_string();
                let op = match scope {
                    R2DeleteScope::PrefixOnly => "delete_r2_prefix_failed",
                    R2DeleteScope::EntireBucket => "delete_r2_bucket_failed",
                };
                self.record_audit(actor, op, &msg);
                self.set_status(format!("R2 削除に失敗しました: {msg}"));
            }
        }

        result
    }

    /// Execute an MCP/Bridge operation by name and return JSON.
    pub fn handle_operation(&self, actor: &str, op: &str, args: &Value) -> AppResult<Value> {
        match op {
            "get_status" => Ok(serde_json::to_value(self.status_report())?),
            "get_config" => Ok(serde_json::to_value(self.get_config())?),
            "get_backup_history" => {
                let limit = args
                    .get("limit")
                    .and_then(Value::as_u64)
                    .unwrap_or(25)
                    .clamp(1, 200) as usize;
                let db = HistoryDb::open(&self.db_path)?;
                Ok(json!({
                    "runs": db.recent_backup_runs(limit)?,
                    "auditLogs": db.recent_audit_logs(limit)?,
                }))
            }
            "list_snapshots" => {
                let cfg = self.get_config();
                let secrets = self.load_secrets()?;
                let host = optional_trimmed_str(args, "host")?;
                let project_key = optional_trimmed_str(args, "projectKey")?;
                let tag = optional_trimmed_str(args, "tag")?;
                let path = optional_trimmed_str(args, "path")?;

                let effective_tag = match (&project_key, &tag) {
                    (Some(pk), _) if !pk.is_empty() => {
                        let valid = validate_project_key(pk)?;
                        Some(format!("{TAG_PROJECT_PREFIX}{valid}"))
                    }
                    (_, Some(t)) if !t.is_empty() => Some(t.clone()),
                    _ => None,
                };
                let mut snaps = runner::list_snapshots_filtered(
                    &cfg,
                    &secrets,
                    host.as_deref().filter(|s| !s.is_empty()),
                    effective_tag.as_deref(),
                    path.as_deref().filter(|s| !s.is_empty()),
                    Some(RETRY_LOCK_READ),
                )?;
                if project_key.as_deref().is_some_and(|s| !s.is_empty())
                    && let Some(extra_tag) = tag.as_deref().filter(|s| !s.is_empty())
                {
                    snaps.retain(|s| s.tags.iter().any(|t| t == extra_tag));
                }
                Ok(json!({ "snapshots": snaps }))
            }
            "list_snapshot_files" => {
                let map = args.as_object().ok_or_else(|| {
                    AppError::InvalidArgument("arguments must be an object".to_owned())
                })?;
                reject_unknown_keys(map, &["snapshotId", "prefix", "maxEntries"])?;
                let snap_id =
                    runner::validate_snapshot_id(required_str(args, "snapshotId")?, true)?;
                let prefix = optional_trimmed_str(args, "prefix")?;
                let max_entries =
                    optional_u64_in_range(args, "maxEntries", 1, 2000)?.unwrap_or(200) as usize;
                let cfg = self.get_config();
                let secrets = self.load_secrets()?;
                let files = runner::list_snapshot_files(
                    &cfg,
                    &secrets,
                    snap_id,
                    prefix.as_deref().filter(|s| !s.is_empty()),
                    max_entries,
                )?;
                Ok(json!({ "snapshotId": snap_id, "entries": files }))
            }
            "get_size_breakdown" => {
                let map = args.as_object().ok_or_else(|| {
                    AppError::InvalidArgument("arguments must be an object".to_owned())
                })?;
                reject_unknown_keys(map, &["snapshotId", "limit"])?;
                let raw_snap = optional_trimmed_str(args, "snapshotId")?;
                let snap_id = runner::validate_snapshot_id(
                    raw_snap
                        .as_deref()
                        .filter(|s| !s.is_empty())
                        .unwrap_or("latest"),
                    true,
                )?;
                let limit = optional_u64_in_range(args, "limit", 1, 500)?.unwrap_or(30) as usize;
                let folders = self.snapshot_folder_sizes(snap_id)?;
                let total: u64 = folders.iter().map(|f| f.bytes).sum();
                Ok(json!({
                    "snapshotId": snap_id,
                    "totalBytes": total,
                    "folderCount": folders.len(),
                    "folders": folders.iter().take(limit).collect::<Vec<_>>(),
                }))
            }
            "diff_snapshots" => {
                let map = args.as_object().ok_or_else(|| {
                    AppError::InvalidArgument("arguments must be an object".to_owned())
                })?;
                reject_unknown_keys(map, &["snapshotA", "snapshotB"])?;
                let snap_a = runner::validate_snapshot_id(required_str(args, "snapshotA")?, true)?;
                let snap_b = runner::validate_snapshot_id(required_str(args, "snapshotB")?, true)?;
                let cfg = self.get_config();
                let secrets = self.load_secrets()?;
                let diff = runner::diff_snapshots(&cfg, &secrets, snap_a, snap_b)?;
                Ok(serde_json::to_value(diff)?)
            }
            "run_backup" => {
                let map = args.as_object().ok_or_else(|| {
                    AppError::InvalidArgument("arguments must be an object".to_owned())
                })?;
                reject_unknown_keys(
                    map,
                    &[
                        "targetIds",
                        "projectKeys",
                        "markHandoffReady",
                        "dryRun",
                        "tags",
                    ],
                )?;
                let dry_run = optional_bool(args, "dryRun")?.unwrap_or(false);
                let target_ids = strict_str_list(args, "targetIds", false, true)?;
                let project_keys = strict_str_list(args, "projectKeys", false, true)?;
                let extra_tags = strict_str_list(args, "tags", true, true)?.unwrap_or_default();
                let mark_handoff_ready = optional_bool(args, "markHandoffReady")?.unwrap_or(false);
                self.execute_backup_with_options(
                    actor,
                    target_ids.as_deref(),
                    project_keys.as_deref(),
                    dry_run,
                    &extra_tags,
                    mark_handoff_ready,
                )
            }
            "cancel_backup" => {
                self.cancel_flag.store(true, Ordering::Relaxed);
                self.record_audit(actor, "cancel_backup", "Requested backup cancellation");
                Ok(json!({ "cancelled": true }))
            }
            "init_repository" => {
                let out = self.init_repository(actor)?;
                Ok(json!({ "initialized": true, "output": out }))
            }
            "check_repository" => {
                let out = match args.get("readDataPercent").and_then(Value::as_u64) {
                    Some(percent) => {
                        self.verify_repository(actor, Some(percent.min(100) as u32))?
                    }
                    None => self.check_repository(actor)?,
                };
                Ok(json!({ "ok": true, "output": out }))
            }
            "update_storage_config" => self.handle_update_storage_config(actor, args),
            "add_backup_target" => {
                let path_str = required_str(args, "path")?;
                let label = args.get("label").and_then(Value::as_str).map(str::to_owned);
                let enabled = optional_bool(args, "enabled")?;
                let excludes = strict_str_list(args, "excludePatterns", true, false)?;
                let tags = strict_str_list(args, "tags", true, false)?;
                let project_key = optional_nullable_str(args, "projectKey")?;
                let writer_state = match args.get("writerState") {
                    None => None,
                    Some(Value::String(s)) => match s.as_str() {
                        "active" => Some(WriterState::Active),
                        "standby" => Some(WriterState::Standby),
                        other => {
                            return Err(AppError::InvalidArgument(format!(
                                "writerState must be \"active\" or \"standby\" (got \"{other}\")"
                            )));
                        }
                    },
                    Some(_) => {
                        return Err(AppError::InvalidArgument(
                            "writerState must be \"active\" or \"standby\"".to_owned(),
                        ));
                    }
                };

                let mut added_target = None;
                let updated = self.update_config(|cfg| {
                    let t = cfg.upsert_target_full(
                        Path::new(path_str),
                        label,
                        excludes,
                        tags,
                        enabled,
                        project_key,
                        writer_state,
                    )?;
                    added_target = Some(t);
                    Ok(())
                })?;
                let target = added_target.expect("update_config ran the closure");
                self.record_audit(
                    actor,
                    "add_backup_target",
                    &format!(
                        "{} ({}) project={:?} writerState={}",
                        target.label,
                        target.path,
                        target.project_key,
                        target.writer_state.as_str()
                    ),
                );
                Ok(json!({
                    "target": target,
                    "totalTargets": updated.targets.len(),
                }))
            }
            "remove_backup_target" => {
                let id_or_path = required_str(args, "idOrPath")?;
                let mut removed = false;
                let updated = self.update_config(|cfg| {
                    removed = cfg.remove_target(id_or_path);
                    Ok(())
                })?;
                self.record_audit(actor, "remove_backup_target", id_or_path);
                Ok(json!({
                    "removed": removed,
                    "totalTargets": updated.targets.len(),
                }))
            }
            "update_schedule" => {
                let map = args.as_object().ok_or_else(|| {
                    AppError::InvalidArgument("arguments must be an object".to_owned())
                })?;
                reject_unknown_keys(
                    map,
                    &[
                        "enabled",
                        "intervalMinutes",
                        "dailyAt",
                        "maintenanceAt",
                        "runOnStartup",
                        "autoPruneAfterBackup",
                        "verifyEveryDays",
                        "verifyReadPercent",
                        "notifyOverdueHours",
                    ],
                )?;
                let enabled = optional_bool(args, "enabled")?;
                let interval_minutes = optional_u64_in_range(
                    args,
                    "intervalMinutes",
                    MIN_INTERVAL_MINUTES,
                    MAX_INTERVAL_MINUTES,
                )?;
                let run_on_startup = optional_bool(args, "runOnStartup")?;
                let auto_prune_after_backup = optional_bool(args, "autoPruneAfterBackup")?;
                let verify_every_days =
                    optional_u64_in_range(args, "verifyEveryDays", 0, 365)?.map(|v| v as u32);
                let verify_read_percent =
                    optional_u64_in_range(args, "verifyReadPercent", 1, 100)?.map(|v| v as u32);
                let notify_overdue_hours =
                    optional_u64_in_range(args, "notifyOverdueHours", 0, 8_760)?.map(|v| v as u32);

                let updated = self.update_config(|cfg| {
                    let prev_auto_prune = cfg.schedule.auto_prune_after_backup;
                    let prev_maint_is_none = cfg.schedule.maintenance_at.is_none();

                    if let Some(en) = enabled {
                        cfg.schedule.enabled = en;
                    }
                    if let Some(mins) = interval_minutes {
                        cfg.schedule.interval_minutes = mins;
                    }
                    if let Some(startup) = run_on_startup {
                        cfg.schedule.run_on_startup = startup;
                    }
                    if let Some(prune) = auto_prune_after_backup {
                        if actor == "mcp" && prune && !cfg.agent.allow_prune {
                            return Err(AppError::InvalidArgument(
                                "MCP cannot enable autoPruneAfterBackup while agent.allowPrune is disabled in config.json"
                                    .to_owned(),
                            ));
                        }
                        cfg.schedule.auto_prune_after_backup = prune;
                    }
                    if let Some(days) = verify_every_days {
                        cfg.schedule.verify_every_days = days;
                    }
                    if let Some(percent) = verify_read_percent {
                        cfg.schedule.verify_read_percent = percent;
                    }
                    if let Some(hours) = notify_overdue_hours {
                        cfg.schedule.notify_overdue_hours = hours;
                    }
                    if let Some(daily) = args.get("dailyAt") {
                        let invalid = || {
                            AppError::InvalidArgument(
                                "dailyAt must be \"HH:MM\" (24-hour), an array of \"HH:MM\", or null"
                                    .to_owned(),
                            )
                        };
                        cfg.schedule.daily_at = match daily {
                            Value::Null => Vec::new(),
                            Value::String(s) => normalize_daily_at([s.as_str()]).ok_or_else(invalid)?,
                            Value::Array(arr) => {
                                let strs: Option<Vec<&str>> = arr.iter().map(Value::as_str).collect();
                                strs.and_then(normalize_daily_at).ok_or_else(invalid)?
                            }
                            _ => return Err(invalid()),
                        };
                    }
                    if let Some(maint) = args.get("maintenanceAt") {
                        let invalid = || {
                            AppError::InvalidArgument(
                                "maintenanceAt must be \"HH:MM\" (24-hour) or null".to_owned(),
                            )
                        };
                        let next_maint = match maint {
                            Value::Null => None,
                            Value::String(s) => {
                                normalize_maintenance_at(Some(s.as_str())).ok_or_else(invalid)?
                            }
                            _ => return Err(invalid()),
                        };
                        if actor == "mcp"
                            && !cfg.agent.allow_prune
                            && next_maint.is_some()
                            && next_maint != cfg.schedule.maintenance_at
                        {
                            return Err(AppError::InvalidArgument(
                                "MCP cannot set or change maintenanceAt while agent.allowPrune is disabled in config.json"
                                    .to_owned(),
                            ));
                        }
                        cfg.schedule.maintenance_at = next_maint;
                    }
                    // Also prevent clearing `maintenanceAt` via MCP from activating post-backup
                    // auto-pruning when `agent.allowPrune` is disabled.
                    if actor == "mcp"
                        && !cfg.agent.allow_prune
                        && cfg.schedule.auto_prune_after_backup
                        && cfg.schedule.maintenance_at.is_none()
                        && !(prev_auto_prune && prev_maint_is_none)
                    {
                        return Err(AppError::InvalidArgument(
                            "MCP cannot activate post-backup auto-pruning while agent.allowPrune is disabled in config.json"
                                .to_owned(),
                        ));
                    }
                    Ok(())
                })?;
                let daily_summary = if updated.schedule.daily_at.is_empty() {
                    "-".to_owned()
                } else {
                    updated.schedule.daily_at.join(",")
                };
                self.record_audit(
                    actor,
                    "update_schedule",
                    &format!(
                        "enabled={} interval={}m daily_at={daily_summary}",
                        updated.schedule.enabled, updated.schedule.interval_minutes,
                    ),
                );
                Ok(serde_json::to_value(updated.schedule)?)
            }
            "update_retention_policy" => {
                let map = args.as_object().ok_or_else(|| {
                    AppError::InvalidArgument("arguments must be an object".to_owned())
                })?;
                reject_unknown_keys(map, &["keepLast", "keepDaily", "keepWeekly", "keepMonthly"])?;
                let keep_last = optional_u32_in_range(args, "keepLast", 0, MAX_KEEP_LAST)?;
                let keep_daily = optional_u32_in_range(args, "keepDaily", 0, MAX_KEEP_DAILY)?;
                let keep_weekly = optional_u32_in_range(args, "keepWeekly", 0, MAX_KEEP_WEEKLY)?;
                let keep_monthly = optional_u32_in_range(args, "keepMonthly", 0, MAX_KEEP_MONTHLY)?;

                let updated = self.update_config(|cfg| {
                    if actor == "mcp" && !cfg.agent.allow_prune {
                        return Err(AppError::InvalidArgument(
                            "MCP update_retention_policy is disabled by agent.allowPrune policy in config.json"
                                .to_owned(),
                        ));
                    }
                    if let Some(v) = keep_last {
                        cfg.retention.keep_last = v;
                    }
                    if let Some(v) = keep_daily {
                        cfg.retention.keep_daily = v;
                    }
                    if let Some(v) = keep_weekly {
                        cfg.retention.keep_weekly = v;
                    }
                    if let Some(v) = keep_monthly {
                        cfg.retention.keep_monthly = v;
                    }
                    Ok(())
                })?;
                self.record_audit(
                    actor,
                    "update_retention_policy",
                    &format!("{:?}", updated.retention),
                );
                Ok(serde_json::to_value(updated.retention)?)
            }
            "restore_snapshot" => {
                let map = args.as_object().ok_or_else(|| {
                    AppError::InvalidArgument("arguments must be an object".to_owned())
                })?;
                reject_unknown_keys(map, &["snapshotId", "targetDir", "includePaths"])?;
                let cfg = self.get_config();
                if actor == "mcp" && !cfg.agent.allow_restore {
                    return Err(AppError::InvalidArgument(
                        "MCP restore is disabled by agent.allowRestore policy in config.json"
                            .to_owned(),
                    ));
                }
                let snap_id =
                    runner::validate_snapshot_id(required_str(args, "snapshotId")?, true)?;
                let target_dir = required_str(args, "targetDir")?;
                let include_paths =
                    strict_str_list(args, "includePaths", false, true)?.unwrap_or_default();

                let msg =
                    self.restore_snapshot(actor, snap_id, Path::new(target_dir), &include_paths)?;
                Ok(json!({ "restored": true, "message": msg }))
            }
            "forget_prune" => {
                let map = args.as_object().ok_or_else(|| {
                    AppError::InvalidArgument("arguments must be an object".to_owned())
                })?;
                reject_unknown_keys(map, &["dryRun", "confirmPrune"])?;
                let cfg = self.get_config();
                if actor == "mcp" && !cfg.agent.allow_prune {
                    return Err(AppError::InvalidArgument(
                        "MCP forget_prune is disabled by agent.allowPrune policy in config.json"
                            .to_owned(),
                    ));
                }
                let dry_run = optional_bool(args, "dryRun")?.unwrap_or(true);
                let confirm_prune = optional_bool(args, "confirmPrune")?.unwrap_or(false);
                if !dry_run && !confirm_prune {
                    return Err(AppError::InvalidArgument(
                        "実削除を実行するには dryRun: false と confirmPrune: true の両方を明示指定してください"
                            .to_owned(),
                    ));
                }
                if dry_run && confirm_prune {
                    return Err(AppError::InvalidArgument(
                        "confirmPrune: true は dryRun: false と組み合わせて指定してください（Dry-Run 確認のみ行う場合は confirmPrune を省略するか false にしてください）"
                            .to_owned(),
                    ));
                }
                let output = self.forget_prune(actor, dry_run)?;
                Ok(json!({ "dryRun": dry_run, "output": output }))
            }
            "delete_r2_bucket" => self.handle_delete_r2_bucket(actor, args),
            "get_project_sync_status" => self.handle_get_project_sync_status(actor, args),
            "release_project" => self.handle_release_project(actor, args),
            "takeover_project" => self.handle_takeover_project(actor, args),
            _ => Err(AppError::InvalidArgument(format!(
                "unsupported operation: {op}"
            ))),
        }
    }

    fn handle_get_project_sync_status(&self, actor: &str, args: &Value) -> AppResult<Value> {
        let map = args
            .as_object()
            .ok_or_else(|| AppError::InvalidArgument("arguments must be an object".to_owned()))?;
        reject_unknown_keys(map, &["projectKey", "refresh", "checkLocalChanges"])?;
        let project_key = optional_trimmed_str(args, "projectKey")?;
        let refresh = args
            .get("refresh")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let check_local_changes = args
            .get("checkLocalChanges")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let report = self.get_project_sync_status(
            actor,
            project_key.as_deref(),
            refresh,
            check_local_changes,
        )?;
        Ok(serde_json::to_value(report)?)
    }

    fn handle_release_project(&self, actor: &str, args: &Value) -> AppResult<Value> {
        let map = args
            .as_object()
            .ok_or_else(|| AppError::InvalidArgument("arguments must be an object".to_owned()))?;
        reject_unknown_keys(map, &["projectKey", "dryRun"])?;
        let project_key = required_str(args, "projectKey")?;
        let dry_run = match args.get("dryRun") {
            None => false,
            Some(v) => v
                .as_bool()
                .ok_or_else(|| AppError::InvalidArgument("dryRun must be a boolean".to_owned()))?,
        };
        self.release_project(actor, project_key, dry_run)
    }

    fn handle_takeover_project(&self, actor: &str, args: &Value) -> AppResult<Value> {
        let map = args
            .as_object()
            .ok_or_else(|| AppError::InvalidArgument("arguments must be an object".to_owned()))?;
        reject_unknown_keys(
            map,
            &[
                "projectKey",
                "localPath",
                "dryRun",
                "saveLocalBackupFirst",
                "confirmDiscardLocalChanges",
                "forceWhileRemoteActive",
                "deleteExtra",
                "confirmProjectKey",
            ],
        )?;
        let project_key = required_str(args, "projectKey")?.to_owned();
        let local_path = optional_trimmed_str(args, "localPath")?;
        let dry_run = match args.get("dryRun") {
            None => true,
            Some(v) => v
                .as_bool()
                .ok_or_else(|| AppError::InvalidArgument("dryRun must be a boolean".to_owned()))?,
        };
        let save_local_backup_first = match args.get("saveLocalBackupFirst") {
            None => true,
            Some(v) => v.as_bool().ok_or_else(|| {
                AppError::InvalidArgument("saveLocalBackupFirst must be a boolean".to_owned())
            })?,
        };
        let discard_local_changes = match args.get("confirmDiscardLocalChanges") {
            None => false,
            Some(v) => v.as_bool().ok_or_else(|| {
                AppError::InvalidArgument("confirmDiscardLocalChanges must be a boolean".to_owned())
            })?,
        };
        let force_while_remote_active = match args.get("forceWhileRemoteActive") {
            None => false,
            Some(v) => v.as_bool().ok_or_else(|| {
                AppError::InvalidArgument("forceWhileRemoteActive must be a boolean".to_owned())
            })?,
        };
        let delete_extra = match args.get("deleteExtra") {
            None => true,
            Some(v) => v.as_bool().ok_or_else(|| {
                AppError::InvalidArgument("deleteExtra must be a boolean".to_owned())
            })?,
        };
        // Keep confirmProjectKey untrimmed so padded/altered strings never pass exact match.
        let confirm_project_key = match args.get("confirmProjectKey") {
            None | Some(Value::Null) => None,
            Some(v) => Some(
                v.as_str()
                    .ok_or_else(|| {
                        AppError::InvalidArgument("confirmProjectKey must be a string".to_owned())
                    })?
                    .to_owned(),
            ),
        };

        let report = self.takeover_project(
            actor,
            TakeoverParams {
                project_key,
                local_path,
                dry_run,
                save_local_backup_first,
                discard_local_changes,
                force_while_remote_active,
                delete_extra,
                confirm_project_key,
                expected_source_snapshot_id: None,
                expected_canonical_path: None,
            },
        )?;
        Ok(serde_json::to_value(report)?)
    }

    fn handle_delete_r2_bucket(&self, actor: &str, args: &Value) -> AppResult<Value> {
        let map = args
            .as_object()
            .ok_or_else(|| AppError::InvalidArgument("arguments must be an object".to_owned()))?;
        reject_unknown_keys(map, &["scope", "dryRun", "confirmBucketName"])?;

        let scope = match required_str(args, "scope")? {
            "prefixOnly" => R2DeleteScope::PrefixOnly,
            "entireBucket" => R2DeleteScope::EntireBucket,
            other => {
                return Err(AppError::InvalidArgument(format!(
                    "scope must be \"prefixOnly\" or \"entireBucket\" (got \"{other}\")"
                )));
            }
        };
        let dry_run = match args.get("dryRun") {
            None => true,
            Some(v) => v
                .as_bool()
                .ok_or_else(|| AppError::InvalidArgument("dryRun must be a boolean".to_owned()))?,
        };
        let confirm = optional_trimmed_str(args, "confirmBucketName")?;
        let report = self.delete_r2_data(actor, scope, confirm.as_deref(), dry_run)?;
        Ok(serde_json::to_value(report)?)
    }

    fn handle_update_storage_config(&self, actor: &str, args: &Value) -> AppResult<Value> {
        let map = args
            .as_object()
            .ok_or_else(|| AppError::InvalidArgument("arguments must be an object".to_owned()))?;

        let requested_backend = match args.get("storageBackend") {
            None => None,
            Some(Value::String(s)) if s == "r2" => Some(crate::config::StorageBackend::R2),
            Some(Value::String(s)) if s == "rclone" => Some(crate::config::StorageBackend::Rclone),
            Some(Value::String(s)) if s == "googleDrive" => {
                return Err(AppError::InvalidArgument(
                    "Google Drive バックエンドは v0.8.0 で廃止されました。storageBackend には \"r2\" または \"rclone\" を指定してください"
                        .to_owned(),
                ));
            }
            Some(Value::String(other)) => {
                return Err(AppError::InvalidArgument(format!(
                    "storageBackend must be \"r2\" or \"rclone\" (got \"{other}\")"
                )));
            }
            Some(_) => {
                return Err(AppError::InvalidArgument(
                    "storageBackend must be \"r2\" or \"rclone\"".to_owned(),
                ));
            }
        };

        if actor == "mcp" && args.get("resticBinaryPath").is_some() {
            return Err(AppError::InvalidArgument(
                "resticBinaryPath cannot be modified via MCP for security reasons; configure it locally in Settings or config.json"
                    .to_owned(),
            ));
        }

        if actor == "mcp"
            && args
                .get("rclone")
                .is_some_and(|v| v.get("binaryPath").is_some())
        {
            return Err(AppError::InvalidArgument(
                "rclone.binaryPath cannot be modified via MCP for security reasons; configure it locally in Settings or config.json"
                    .to_owned(),
            ));
        }

        reject_unknown_keys(map, &["storageBackend", "r2", "rclone", "resticBinaryPath"])?;

        let rclone_patch = args.get("rclone");
        if let Some(rclone_val) = rclone_patch {
            let rclone_map = rclone_val
                .as_object()
                .ok_or_else(|| AppError::InvalidArgument("rclone must be an object".to_owned()))?;
            reject_unknown_keys(
                rclone_map,
                &["remote", "bucket", "port", "binaryPath", "configPath"],
            )?;
        }

        let restic_path = optional_nullable_str(args, "resticBinaryPath")?;
        if let Some(Some(ref p)) = restic_path
            && !Path::new(p).is_file()
        {
            return Err(AppError::InvalidArgument(format!(
                "resticBinaryPath does not point to an existing file: {p}"
            )));
        }

        let r2_patch = args.get("r2");
        if let Some(r2_val) = r2_patch {
            let r2_map = r2_val
                .as_object()
                .ok_or_else(|| AppError::InvalidArgument("r2 must be an object".to_owned()))?;
            reject_unknown_keys(
                r2_map,
                &[
                    "accountId",
                    "bucket",
                    "prefix",
                    "endpointOverride",
                    "region",
                    "maxConnections",
                ],
            )?;
        }

        let updated = self.update_config(|cfg| {
            let mut next = cfg.clone();
            if let Some(path) = restic_path {
                next.restic_binary_path = path;
            }
            if let Some(backend) = requested_backend {
                next.storage_backend = backend;
            }
            if let Some(r2_val) = r2_patch {
                if let Some(v) = optional_trimmed_str(r2_val, "accountId")? {
                    next.r2.account_id = v;
                }
                if let Some(v) = optional_trimmed_str(r2_val, "bucket")? {
                    next.r2.bucket = v;
                }
                if let Some(v) = optional_trimmed_str(r2_val, "prefix")? {
                    next.r2.prefix = v;
                }
                if let Some(v) = optional_nullable_str(r2_val, "endpointOverride")? {
                    next.r2.endpoint_override = v;
                }
                if let Some(v) = optional_trimmed_str(r2_val, "region")? {
                    next.r2.region = if v.is_empty() { "auto".to_owned() } else { v };
                }
                if let Some(v) = optional_u32_clamped(r2_val, "maxConnections", 1, 32)? {
                    next.r2.max_connections = v;
                }
            }
            if let Some(rclone_val) = rclone_patch {
                if let Some(v) = optional_trimmed_str(rclone_val, "remote")? {
                    next.rclone.remote = v;
                }
                if let Some(v) = optional_trimmed_str(rclone_val, "bucket")? {
                    next.rclone.bucket = v;
                }
                if let Some(v) = optional_nullable_str(rclone_val, "port")? {
                    next.rclone.port = v.and_then(|p| p.trim().parse::<u16>().ok());
                }
                if let Some(v) = optional_nullable_str(rclone_val, "configPath")? {
                    next.rclone.config_path = v;
                }
                if let Some(v) = optional_nullable_str(rclone_val, "binaryPath")? {
                    next.rclone.binary_path = v;
                }
            }
            if next.storage_backend == crate::config::StorageBackend::Rclone {
                next.rclone.validate_remote().map_err(|e| {
                    AppError::InvalidArgument(format!("rclone.remote が無効です: {e}"))
                })?;
            }

            *cfg = next;
            Ok(())
        })?;

        let repo_url = updated.repository_url().ok();
        self.record_audit(
            actor,
            "update_storage_config",
            &format!("backend=R2 repo={}", repo_url.as_deref().unwrap_or("-")),
        );

        Ok(json!({
            "storageBackend": "r2",
            "storageConfigured": updated.is_storage_configured(),
            "repositoryUrl": repo_url,
            "r2": updated.r2,
            "resticBinaryPath": updated.restic_binary_path,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_controller() -> (tempfile::TempDir, Arc<SharedController>) {
        let dir = tempfile::tempdir().expect("tempdir");
        let cfg_path = dir.path().join("config.json");
        let ctrl = SharedController::new(dir.path().to_path_buf(), cfg_path).expect("controller");
        (dir, ctrl)
    }

    #[test]
    fn update_storage_config_updates_r2_and_reports_r2_only_surface() {
        let (_dir, ctrl) = test_controller();
        let res = ctrl
            .handle_operation(
                "mcp",
                "update_storage_config",
                &json!({
                    "storageBackend": "r2",
                    "r2": {
                        "accountId": "acc123",
                        "bucket": "my-backups",
                        "prefix": "pc-01",
                        "maxConnections": 8
                    }
                }),
            )
            .expect("update_storage_config");

        assert_eq!(res["storageBackend"], "r2");
        assert_eq!(res["storageConfigured"], true);
        assert_eq!(
            res["repositoryUrl"],
            "s3:https://acc123.r2.cloudflarestorage.com/my-backups/pc-01"
        );
        assert_eq!(res["r2"]["bucket"], "my-backups");
        assert_eq!(res["r2"]["maxConnections"], 8);
        assert!(res.get("googleDrive").is_none());
        assert!(res.get("rcloneBinaryPath").is_none());

        // Reload from disk to confirm atomic persistence.
        let reloaded = ctrl.reload_config_from_disk().expect("reload");
        assert_eq!(reloaded.r2.account_id, "acc123");
        assert_eq!(reloaded.r2.bucket, "my-backups");
        assert_eq!(reloaded.r2.prefix, "pc-01");

        // get_status keeps fixed storageBackend="r2" and omits removed Drive/rclone fields.
        let status = ctrl
            .handle_operation("mcp", "get_status", &json!({}))
            .expect("get_status");
        assert_eq!(status["storageBackend"], "r2");
        assert!(status.get("rcloneBinary").is_none());
        assert!(status.get("activeGdriveDelete").is_none());
        assert!(
            status["secretsReadiness"]
                .get("hasGdriveOauthToken")
                .is_none()
        );
        assert!(
            status["secretsReadiness"]
                .get("hasGdriveClientSecret")
                .is_none()
        );

        // get_config omits googleDrive and rcloneBinaryPath.
        let cfg_json = ctrl
            .handle_operation("mcp", "get_config", &json!({}))
            .expect("get_config");
        assert!(cfg_json.get("googleDrive").is_none());
        assert!(cfg_json.get("rcloneBinaryPath").is_none());
        assert!(cfg_json.get("storageBackend").is_none());

        // Clearing optional endpointOverride with null works.
        ctrl.handle_operation(
            "mcp",
            "update_storage_config",
            &json!({ "r2": { "endpointOverride": "https://example.invalid" } }),
        )
        .expect("set endpointOverride");
        let cleared = ctrl
            .handle_operation(
                "mcp",
                "update_storage_config",
                &json!({ "r2": { "endpointOverride": null } }),
            )
            .expect("clear endpointOverride");
        assert!(cleared["r2"]["endpointOverride"].is_null());

        let db = HistoryDb::open(&ctrl.db_path).expect("db");
        let logs = db.recent_audit_logs(10).expect("audit logs");
        assert!(logs.iter().any(|l| l.operation == "update_storage_config"));
    }

    #[test]
    fn update_storage_config_rejects_removed_google_drive_and_invalid_inputs() {
        let (_dir, ctrl) = test_controller();

        let err = ctrl
            .handle_operation(
                "mcp",
                "update_storage_config",
                &json!({ "storageBackend": "googleDrive" }),
            )
            .unwrap_err();
        assert!(err.to_string().contains("廃止"));

        // Legacy MCP payloads combining storageBackend="googleDrive" with googleDrive also get the
        // dedicated removal error rather than an opaque unknown-key error.
        let combo_err = ctrl
            .handle_operation(
                "mcp",
                "update_storage_config",
                &json!({
                    "storageBackend": "googleDrive",
                    "googleDrive": { "folder": "restic-tray" }
                }),
            )
            .unwrap_err();
        assert!(combo_err.to_string().contains("廃止"));

        assert!(
            ctrl.handle_operation(
                "mcp",
                "update_storage_config",
                &json!({ "storageBackend": "s3" })
            )
            .is_err()
        );
        let err = ctrl
            .handle_operation(
                "mcp",
                "update_storage_config",
                &json!({ "googleDrive": { "folder": "restic-tray" } }),
            )
            .unwrap_err();
        assert!(
            err.to_string()
                .contains("unknown field in arguments: googleDrive")
        );
        let err = ctrl
            .handle_operation(
                "mcp",
                "update_storage_config",
                &json!({ "rcloneBinaryPath": "Z:\\definitely-nonexistent\\rclone.exe" }),
            )
            .unwrap_err();
        assert!(
            err.to_string()
                .contains("unknown field in arguments: rcloneBinaryPath")
        );
        assert!(
            ctrl.handle_operation(
                "mcp",
                "update_storage_config",
                &json!({ "resticBinaryPath": "Z:\\definitely-nonexistent\\restic.exe" })
            )
            .is_err()
        );
        // Secret fields are unknown and rejected.
        assert!(
            ctrl.handle_operation(
                "mcp",
                "update_storage_config",
                &json!({ "resticPassword": "secret" })
            )
            .is_err()
        );
        // Removed MCP operation is rejected.
        assert!(
            ctrl.handle_operation(
                "mcp",
                "delete_google_drive_repo",
                &json!({ "dryRun": true })
            )
            .is_err()
        );
    }

    #[test]
    fn init_and_check_repository_record_audit_log_on_failure() {
        let (_dir, ctrl) = test_controller();
        // Default config has no R2 account/bucket or password, so both fail immediately.
        assert!(ctrl.init_repository("gui").is_err());
        assert!(ctrl.check_repository("gui").is_err());

        let db = HistoryDb::open(&ctrl.db_path).expect("db");
        let logs = db.recent_audit_logs(10).expect("logs");
        assert!(logs.iter().any(|l| l.operation == "init_repository_failed"));
        assert!(
            logs.iter()
                .any(|l| l.operation == "check_repository_failed")
        );
    }

    #[test]
    fn delete_r2_bucket_mcp_enforces_opt_in_policy_and_confirmation() {
        let (_dir, ctrl) = test_controller();
        ctrl.update_config(|cfg| {
            cfg.r2.account_id = "acc123".to_owned();
            cfg.r2.bucket = "my-r2-bucket".to_owned();
            cfg.r2.prefix = "workstation".to_owned();
            Ok(())
        })
        .unwrap();

        // 1. Disabled by default (`agent.allowDeleteStorage == false`).
        let err = ctrl
            .handle_operation(
                "mcp",
                "delete_r2_bucket",
                &json!({ "scope": "entireBucket", "dryRun": true }),
            )
            .unwrap_err();
        assert!(err.to_string().contains("allowDeleteStorage"));

        // 2. Enable opt-in policy.
        ctrl.update_config(|cfg| {
            cfg.agent.allow_delete_storage = true;
            Ok(())
        })
        .unwrap();

        // 3. Real delete without matching confirmBucketName is rejected.
        let err = ctrl
            .handle_operation(
                "mcp",
                "delete_r2_bucket",
                &json!({
                    "scope": "entireBucket",
                    "dryRun": false,
                    "confirmBucketName": "wrong-bucket"
                }),
            )
            .unwrap_err();
        assert!(err.to_string().contains("confirmBucketName"));

        // 4. Missing confirmBucketName on dryRun: false is rejected.
        assert!(
            ctrl.handle_operation(
                "mcp",
                "delete_r2_bucket",
                &json!({ "scope": "prefixOnly", "dryRun": false }),
            )
            .is_err()
        );

        // 5. prefixOnly with empty prefix is rejected even on dryRun: true.
        ctrl.update_config(|cfg| {
            cfg.r2.prefix.clear();
            Ok(())
        })
        .unwrap();
        let err = ctrl
            .handle_operation(
                "mcp",
                "delete_r2_bucket",
                &json!({ "scope": "prefixOnly", "dryRun": true }),
            )
            .unwrap_err();
        assert!(err.to_string().contains("プレフィックス"));

        // 6. Invalid scope or unknown field is rejected.
        assert!(
            ctrl.handle_operation("mcp", "delete_r2_bucket", &json!({ "scope": "everything" }),)
                .is_err()
        );
        assert!(
            ctrl.handle_operation(
                "mcp",
                "delete_r2_bucket",
                &json!({ "scope": "entireBucket", "extra": 1 }),
            )
            .is_err()
        );
    }

    #[test]
    fn multi_pc_mcp_tools_enforce_handoff_policy_git_and_unity_locks() {
        let (dir, ctrl) = test_controller();
        let proj_dir = dir.path().join("ProjectAlpha");
        let git_dir = dir.path().join("GitProj");
        std::fs::create_dir_all(&proj_dir).unwrap();
        std::fs::create_dir_all(git_dir.join(".git")).unwrap();

        // add_backup_target with projectKey & writerState.
        let added = ctrl
            .handle_operation(
                "mcp",
                "add_backup_target",
                &json!({
                    "path": proj_dir.to_string_lossy(),
                    "projectKey": "project-alpha",
                    "writerState": "active"
                }),
            )
            .expect("add shared target");
        assert_eq!(added["target"]["projectKey"], "project-alpha");
        assert!(added["target"].get("writerState").is_none());
        assert_eq!(
            ctrl.get_config()
                .find_target_by_project_key("project-alpha")
                .unwrap()
                .writer_state,
            WriterState::Active
        );

        let standby = ctrl
            .handle_operation(
                "mcp",
                "add_backup_target",
                &json!({
                    "path": proj_dir.to_string_lossy(),
                    "projectKey": "project-alpha",
                    "writerState": "standby"
                }),
            )
            .expect("switch target to standby");
        assert_eq!(standby["target"]["writerState"], "standby");
        ctrl.handle_operation(
            "mcp",
            "add_backup_target",
            &json!({
                "path": proj_dir.to_string_lossy(),
                "projectKey": "project-alpha",
                "writerState": "active"
            }),
        )
        .expect("switch target back to active");

        // Git project (both direct and nested subfolder inside a git repo) rejects projectKey.
        let git_sub = git_dir.join("sub").join("UnityProj");
        std::fs::create_dir_all(&git_sub).unwrap();
        for p in [&git_dir, &git_sub] {
            assert!(
                ctrl.handle_operation(
                    "mcp",
                    "add_backup_target",
                    &json!({
                        "path": p.to_string_lossy(),
                        "projectKey": "git-proj"
                    }),
                )
                .is_err()
            );
        }

        // Reserved user tags (`project:`, `handoff:`, `excludes:`, commas) are rejected.
        for bad_tag in ["handoff:released", "project:foo", "excludes:abc", "a,b"] {
            assert!(
                ctrl.handle_operation(
                    "mcp",
                    "add_backup_target",
                    &json!({
                        "path": proj_dir.to_string_lossy(),
                        "tags": [bad_tag]
                    }),
                )
                .is_err(),
                "reserved tag {bad_tag} must be rejected"
            );
        }

        // update_schedule rejects setting maintenanceAt by default (`agent.allowPrune == false`)
        // and accepts it once `agent.allowPrune` is opted in.
        let err = ctrl
            .handle_operation(
                "mcp",
                "update_schedule",
                &json!({ "maintenanceAt": "05:00" }),
            )
            .unwrap_err();
        assert!(err.to_string().contains("allowPrune"));
        ctrl.update_config(|c| {
            c.agent.allow_prune = true;
            Ok(())
        })
        .unwrap();
        let sched = ctrl
            .handle_operation(
                "mcp",
                "update_schedule",
                &json!({ "maintenanceAt": "05:00" }),
            )
            .expect("update maintenanceAt");
        assert_eq!(sched["maintenanceAt"], "05:00");

        // get_status exposes machineName, maintenanceRole, projectSyncSummary.
        // A newly added shared target before remote refresh is classified as `unchecked` (not `localLatest`).
        let status = ctrl
            .handle_operation("mcp", "get_status", &json!({}))
            .expect("get_status");
        assert!(status["machineName"].is_string());
        assert_eq!(status["maintenanceRole"], "primary");
        assert_eq!(status["projectSyncSummary"]["unchecked"], 1);
        assert_eq!(status["projectSyncSummary"]["localLatest"], 0);

        // get_project_sync_status returns cached project list and filters by projectKey.
        let sync_res = ctrl
            .handle_operation(
                "mcp",
                "get_project_sync_status",
                &json!({ "projectKey": "project-alpha" }),
            )
            .expect("get_project_sync_status");
        assert_eq!(sync_res["projects"].as_array().unwrap().len(), 1);
        assert_eq!(sync_res["projects"][0]["projectKey"], "project-alpha");

        // release_project blocks when Temp\UnityLockfile exists.
        std::fs::create_dir_all(proj_dir.join("Temp")).unwrap();
        std::fs::write(proj_dir.join("Temp").join("UnityLockfile"), b"").unwrap();
        let err = ctrl
            .handle_operation(
                "mcp",
                "release_project",
                &json!({ "projectKey": "project-alpha" }),
            )
            .unwrap_err();
        assert!(err.to_string().contains("UnityLockfile"));

        // takeover_project is disabled by default (`agent.allowHandoff == false`).
        let err = ctrl
            .handle_operation(
                "mcp",
                "takeover_project",
                &json!({ "projectKey": "project-alpha", "dryRun": true }),
            )
            .unwrap_err();
        assert!(err.to_string().contains("allowHandoff"));

        // Enabling allowHandoff still requires exact untrimmed confirmProjectKey when dryRun: false.
        ctrl.update_config(|c| {
            c.agent.allow_handoff = true;
            Ok(())
        })
        .unwrap();
        for bad_confirm in ["wrong-key", " project-alpha", "project-alpha "] {
            let err = ctrl
                .handle_operation(
                    "mcp",
                    "takeover_project",
                    &json!({
                        "projectKey": "project-alpha",
                        "dryRun": false,
                        "confirmProjectKey": bad_confirm
                    }),
                )
                .unwrap_err();
            assert!(
                err.to_string().contains("confirmProjectKey"),
                "confirm={bad_confirm:?} must fail confirmProjectKey check: {err}"
            );
        }
    }

    fn test_restic_bin() -> Option<String> {
        if let Ok(path) = std::env::var("RESTIC_TEST_BIN")
            && !path.trim().is_empty()
            && std::path::Path::new(&path).is_file()
        {
            return Some(path);
        }
        let info = probe_restic_binary(None);
        info.available.then_some(info.resolved_path?)
    }

    fn configure_test_pc(
        ctrl: &Arc<SharedController>,
        machine_name: &str,
        default_root: &std::path::Path,
        role: MaintenanceRole,
        restic_bin: &str,
    ) {
        ctrl.update_config(|cfg| {
            cfg.machine_name = Some(machine_name.to_owned());
            cfg.default_project_root = Some(default_root.to_string_lossy().into_owned());
            cfg.maintenance_role = role;
            cfg.restic_binary_path = Some(restic_bin.to_owned());
            cfg.r2.account_id = "test-acc".to_owned();
            cfg.r2.bucket = "test-bucket".to_owned();
            cfg.r2.prefix = "test-prefix".to_owned();
            cfg.agent.allow_handoff = true;
            cfg.retention.keep_last = 2;
            cfg.retention.keep_daily = 0;
            cfg.retention.keep_weekly = 0;
            cfg.retention.keep_monthly = 0;
            Ok(())
        })
        .expect("configure test pc");

        let mut secrets = StoredSecrets::default();
        secrets.r2_access_key_id = Some("ak".to_owned());
        secrets.r2_secret_access_key = Some("sk".to_owned());
        secrets.restic_password = Some("test-repo-pass-123".to_owned());
        ctrl.save_secrets(&secrets).expect("save test secrets");
    }

    #[test]
    #[ignore = "requires restic.exe (run scripts/download-restic.ps1, or pass --ignored)"]
    fn end_to_end_two_pc_handoff_restore_delete_forget_and_legacy_with_real_restic() {
        let restic_bin = test_restic_bin().expect(
            "restic.exe is required for this E2E test; run scripts/download-restic.ps1 or set RESTIC_TEST_BIN",
        );

        let workspace = tempfile::tempdir().expect("workspace tempdir");
        let repo_dir = workspace.path().join("shared-repo");
        let _repo_guard = crate::restic::runner::TestLocalRepoGuard::new(&repo_dir);

        let (_dir_a, pc_a) = test_controller();
        let (_dir_b, pc_b) = test_controller();

        let root_a = workspace.path().join("pc_a_root");
        let root_b = workspace.path().join("pc_b_root");
        std::fs::create_dir_all(&root_a).unwrap();
        std::fs::create_dir_all(&root_b).unwrap();

        configure_test_pc(
            &pc_a,
            "pc-a",
            &root_a,
            MaintenanceRole::Primary,
            &restic_bin,
        );
        configure_test_pc(&pc_b, "pc-b", &root_b, MaintenanceRole::None, &restic_bin);

        // Initialize shared local Restic repository via production controller path.
        pc_a.init_repository("test").expect("init repo");

        // 1. Create a Unity project on pc-a with Library/, SerializedUdonPrograms/, and CACHEDIR.TAG.
        let proj_a = root_a.join("ProjectAlpha");
        std::fs::create_dir_all(proj_a.join("Assets").join("SerializedUdonPrograms")).unwrap();
        std::fs::create_dir_all(proj_a.join("ProjectSettings")).unwrap();
        std::fs::create_dir_all(proj_a.join("Library")).unwrap();
        std::fs::create_dir_all(proj_a.join("CacheDir")).unwrap();
        std::fs::write(proj_a.join("Assets").join("scene.txt"), "pc-a-v1").unwrap();
        std::fs::write(proj_a.join("Assets").join("old.txt"), "will-delete").unwrap();
        std::fs::write(
            proj_a.join("ProjectSettings").join("ProjectVersion.txt"),
            "m_EditorVersion: 2022.3.22f1\n",
        )
        .unwrap();
        std::fs::write(proj_a.join("Library").join("pc-a-cache.bin"), "PC-A-CACHE").unwrap();
        std::fs::write(
            proj_a
                .join("Assets")
                .join("SerializedUdonPrograms")
                .join("pc-a-udon.bin"),
            "PC-A-UDON",
        )
        .unwrap();
        std::fs::write(
            proj_a.join("CacheDir").join("CACHEDIR.TAG"),
            "Signature: 8a477f597d28d172789f06886806bc55\n",
        )
        .unwrap();
        std::fs::write(proj_a.join("CacheDir").join("keep.bin"), "PC-A-CACHEDIR").unwrap();

        pc_a.handle_operation(
            "mcp",
            "add_backup_target",
            &json!({
                "path": proj_a.to_string_lossy(),
                "projectKey": "project-alpha",
                "writerState": "active"
            }),
        )
        .expect("register project-alpha on pc-a");

        // Release from pc-a -> creates snapshot with --host pc-a, project:project-alpha, handoff:released, excludes:<fp>
        let rel_a = pc_a
            .release_project("test", "project-alpha", false)
            .expect("release from pc-a");
        assert_eq!(rel_a["writerState"], "standby");
        assert_eq!(
            pc_a.get_config()
                .find_target_by_project_key("project-alpha")
                .unwrap()
                .writer_state,
            WriterState::Standby
        );
        std::thread::sleep(Duration::from_millis(50));

        // 2. pc-b refreshes sync status -> sees project-alpha as `not_local` and observes host `pc-a`.
        let sync_b1 = pc_b
            .refresh_project_sync_status("test", false)
            .expect("refresh on pc-b");
        assert_eq!(sync_b1.summary.not_local, 1);
        assert_eq!(sync_b1.projects.len(), 1);
        assert_eq!(
            sync_b1.projects[0].project_key.as_deref(),
            Some("project-alpha")
        );
        assert_eq!(
            sync_b1.projects[0].state,
            sync::ProjectSyncStateKind::NotLocal
        );
        assert_eq!(sync_b1.projects[0].latest_host.as_deref(), Some("pc-a"));
        assert!(sync_b1.projects[0].handoff_ready);

        // 3. pc-b dry-run takeover suggests defaultProjectRoot\ProjectAlpha and does not write files.
        let proj_b = root_b.join("ProjectAlpha");
        let dry_b = pc_b
            .takeover_project(
                "test",
                sync::TakeoverParams {
                    project_key: "project-alpha".to_owned(),
                    local_path: None,
                    delete_extra: true,
                    save_local_backup_first: true,
                    discard_local_changes: false,
                    force_while_remote_active: false,
                    dry_run: true,
                    confirm_project_key: None,
                    expected_source_snapshot_id: None,
                    expected_canonical_path: None,
                },
            )
            .expect("dry-run takeover on pc-b");
        assert!(dry_b.dry_run);
        assert!(!proj_b.exists());
        assert_eq!(
            PathBuf::from(&dry_b.local_path).file_name().unwrap(),
            "ProjectAlpha"
        );

        // Real takeover on pc-b -> restores files, saves handoff:claimed, becomes Active/local_latest.
        let take_b = pc_b
            .takeover_project(
                "test",
                sync::TakeoverParams {
                    project_key: "project-alpha".to_owned(),
                    local_path: None,
                    delete_extra: true,
                    save_local_backup_first: true,
                    discard_local_changes: false,
                    force_while_remote_active: false,
                    dry_run: false,
                    confirm_project_key: Some("project-alpha".to_owned()),
                    expected_source_snapshot_id: Some(dry_b.source_snapshot_id.clone()),
                    expected_canonical_path: Some(dry_b.local_path.clone()),
                },
            )
            .expect("real takeover on pc-b");
        assert!(!take_b.dry_run);
        assert!(take_b.claimed_snapshot_id.is_some());
        assert_eq!(
            std::fs::read_to_string(proj_b.join("Assets").join("scene.txt")).unwrap(),
            "pc-a-v1"
        );
        assert!(!proj_b.join("Library").exists());
        assert!(!proj_b.join("CacheDir").join("keep.bin").exists());
        assert_eq!(
            pc_b.get_config()
                .find_target_by_project_key("project-alpha")
                .unwrap()
                .writer_state,
            WriterState::Active
        );
        // Releasing from standby PC (pc-a) must be rejected.
        assert!(
            pc_a.release_project("test", "project-alpha", false)
                .is_err()
        );
        std::thread::sleep(Duration::from_millis(50));

        // 4. pc-b edits scene.txt, deletes old.txt, creates its own Library, and releases.
        std::fs::write(proj_b.join("Assets").join("scene.txt"), "pc-b-v2").unwrap();
        std::fs::remove_file(proj_b.join("Assets").join("old.txt")).unwrap();
        std::fs::create_dir_all(proj_b.join("Library")).unwrap();
        std::fs::write(proj_b.join("Library").join("pc-b-cache.bin"), "PC-B-CACHE").unwrap();
        pc_b.release_project("test", "project-alpha", false)
            .expect("release from pc-b");
        std::thread::sleep(Duration::from_millis(50));

        // 5a. Before local edits on pc-a, sync with check_local_changes=true sees `RemoteNewer`.
        let sync_a_clean = pc_a
            .refresh_project_sync_status("test", true)
            .expect("refresh on pc-a before local edits");
        assert_eq!(
            sync_a_clean.projects[0].state,
            sync::ProjectSyncStateKind::RemoteNewer
        );
        assert_eq!(
            sync_a_clean.projects[0].latest_host.as_deref(),
            Some("pc-b")
        );

        // 5b. Creating unsaved local edits on pc-a flips check_local_changes=true to `Diverged`.
        std::fs::write(
            proj_a.join("Assets").join("local-draft.txt"),
            "unsaved-draft",
        )
        .unwrap();
        let sync_a = pc_a
            .refresh_project_sync_status("test", true)
            .expect("refresh on pc-a with local edits");
        assert_eq!(
            sync_a.projects[0].state,
            sync::ProjectSyncStateKind::Diverged
        );
        assert!(sync_a.projects[0].has_unbacked_local_changes);

        // Pointing localPath to a different non-empty folder detects unbacked
        // changes on that folder (even if registered path A had none).
        let alt_dest = root_a.join("AltProjectAlpha");
        std::fs::create_dir_all(&alt_dest).unwrap();
        std::fs::write(alt_dest.join("only-in-alt.txt"), "precious").unwrap();
        let alt_preview = pc_a
            .takeover_project(
                "test",
                sync::TakeoverParams {
                    project_key: "project-alpha".to_owned(),
                    local_path: Some(alt_dest.to_string_lossy().into_owned()),
                    delete_extra: true,
                    save_local_backup_first: true,
                    discard_local_changes: false,
                    force_while_remote_active: false,
                    dry_run: true,
                    confirm_project_key: None,
                    expected_source_snapshot_id: None,
                    expected_canonical_path: None,
                },
            )
            .expect("alt path preview");
        assert!(alt_preview.has_unbacked_local_changes);
        assert!(alt_preview.will_save_safety_backup);
        std::fs::remove_dir_all(&alt_dest).unwrap();

        // MCP saveLocalBackupFirst=false without confirmDiscardLocalChanges=true must be blocked.
        let blocked = pc_a
            .handle_operation(
                "mcp",
                "takeover_project",
                &json!({
                    "projectKey": "project-alpha",
                    "dryRun": false,
                    "saveLocalBackupFirst": false,
                    "confirmProjectKey": "project-alpha"
                }),
            )
            .unwrap_err();
        assert!(
            blocked.to_string().contains("未バックアップ")
                || blocked.to_string().contains("confirmDiscardLocalChanges"),
            "unexpected error: {blocked}"
        );

        // Windows bracket marker folders `[Cache]` and `cache]` must survive
        // `restore --delete` (no backslash escaping on Windows).
        for special_dir in ["[Cache]", "cache]"] {
            let sd = proj_a.join(special_dir);
            std::fs::create_dir_all(&sd).unwrap();
            std::fs::write(
                sd.join("CACHEDIR.TAG"),
                "Signature: 8a477f597d28d172789f06886806bc55\n",
            )
            .unwrap();
            std::fs::write(sd.join("keep-special.bin"), "SPECIAL-OK").unwrap();
        }

        // Taking over WITH save_local_backup_first: true and delete_extra: true saves safety-backup,
        // mirrors deletions (old.txt & local-draft.txt deleted), and PRESERVES Unity Library,
        // SerializedUdonPrograms, CACHEDIR.TAG CacheDir, `[Cache]`, and `cache]`!
        let take_a = pc_a
            .takeover_project(
                "test",
                sync::TakeoverParams {
                    project_key: "project-alpha".to_owned(),
                    local_path: None,
                    delete_extra: true,
                    save_local_backup_first: true,
                    discard_local_changes: false,
                    force_while_remote_active: false,
                    dry_run: false,
                    confirm_project_key: Some("project-alpha".to_owned()),
                    expected_source_snapshot_id: None,
                    expected_canonical_path: None,
                },
            )
            .expect("takeover back on pc-a with safety backup");
        let safety_id = take_a
            .safety_backup_snapshot_id
            .clone()
            .expect("safety backup must be created");
        assert!(take_a.claimed_snapshot_id.is_some());
        assert_eq!(
            std::fs::read_to_string(proj_a.join("Assets").join("scene.txt")).unwrap(),
            "pc-b-v2"
        );
        assert!(
            !proj_a.join("Assets").join("old.txt").exists(),
            "file deleted on pc-b must be deleted on pc-a by --delete"
        );
        assert!(
            !proj_a.join("Assets").join("local-draft.txt").exists(),
            "local-draft.txt must be removed after safety-backup + --delete restore"
        );
        assert_eq!(
            std::fs::read_to_string(proj_a.join("Library").join("pc-a-cache.bin")).unwrap(),
            "PC-A-CACHE",
            "Unity Library must survive restore --delete"
        );
        assert_eq!(
            std::fs::read_to_string(
                proj_a
                    .join("Assets")
                    .join("SerializedUdonPrograms")
                    .join("pc-a-udon.bin")
            )
            .unwrap(),
            "PC-A-UDON",
            "Unity SerializedUdonPrograms must survive restore --delete"
        );
        assert_eq!(
            std::fs::read_to_string(proj_a.join("CacheDir").join("keep.bin")).unwrap(),
            "PC-A-CACHEDIR",
            "CACHEDIR.TAG folder contents must survive restore --delete"
        );
        for special_dir in ["[Cache]", "cache]"] {
            assert_eq!(
                std::fs::read_to_string(proj_a.join(special_dir).join("keep-special.bin")).unwrap(),
                "SPECIAL-OK",
                "special glob folder {special_dir} must survive restore --delete on Windows"
            );
        }
        std::thread::sleep(Duration::from_millis(50));

        // `delete_extra: false` must still honor receiver-only excludes so a
        // locally excluded file is not overwritten even if the sender snapshot contains the same path.
        pc_a.update_config(|c| {
            c.find_target_by_project_key_mut("project-alpha")
                .unwrap()
                .exclude_patterns
                .push("Assets/scene.txt".to_owned());
            Ok(())
        })
        .unwrap();
        std::fs::write(
            proj_a.join("Assets").join("scene.txt"),
            "local-excluded-keep",
        )
        .unwrap();
        let take_a_nodel = pc_a
            .takeover_project(
                "test",
                sync::TakeoverParams {
                    project_key: "project-alpha".to_owned(),
                    local_path: None,
                    delete_extra: false,
                    save_local_backup_first: false,
                    discard_local_changes: true,
                    force_while_remote_active: false,
                    dry_run: false,
                    confirm_project_key: Some("project-alpha".to_owned()),
                    expected_source_snapshot_id: None,
                    expected_canonical_path: None,
                },
            )
            .expect("takeover with delete_extra=false must succeed");
        assert_eq!(
            std::fs::read_to_string(proj_a.join("Assets").join("scene.txt")).unwrap(),
            "local-excluded-keep",
            "receiver-excluded file must not be overwritten when delete_extra=false"
        );
        // Restore exclude_patterns back so subsequent tests match normal project setup.
        pc_a.update_config(|c| {
            c.find_target_by_project_key_mut("project-alpha")
                .unwrap()
                .exclude_patterns
                .clear();
            Ok(())
        })
        .unwrap();
        let latest_id_before_interrupt = take_a_nodel
            .claimed_snapshot_id
            .clone()
            .expect("claimed snapshot after delete_extra=false");
        std::thread::sleep(Duration::from_millis(50));

        // 6a. Interrupted `restoring` on the same (source_id, safe_dest) refuses
        // silent discard without safety backup or explicit discard confirmation, and converges
        // cleanly when `save_local_backup_first: false, discard_local_changes: true` (preserving
        // prior `safety_backup_snapshot_id`).
        let proj_a_canon = dunce::canonicalize(&proj_a)
            .unwrap()
            .to_string_lossy()
            .into_owned();
        {
            let db = HistoryDb::open(&pc_a.db_path).unwrap();
            db.set_handoff_state(
                "project-alpha",
                "restoring",
                Some(&latest_id_before_interrupt),
                Some(&proj_a_canon),
                Some(&safety_id),
            )
            .unwrap();
        }
        let interrupted = pc_a
            .get_project_sync_status("test", Some("project-alpha"), false, false)
            .unwrap();
        assert_eq!(
            interrupted.projects[0].state,
            sync::ProjectSyncStateKind::Diverged
        );
        assert_eq!(interrupted.projects[0].handoff_status, "restoring");
        // Without save_local_backup_first or discard_local_changes, interrupted restoring must refuse.
        assert!(
            pc_a.takeover_project(
                "test",
                sync::TakeoverParams {
                    project_key: "project-alpha".to_owned(),
                    local_path: None,
                    delete_extra: true,
                    save_local_backup_first: false,
                    discard_local_changes: false,
                    force_while_remote_active: false,
                    dry_run: false,
                    confirm_project_key: Some("project-alpha".to_owned()),
                    expected_source_snapshot_id: None,
                    expected_canonical_path: None,
                },
            )
            .is_err(),
            "interrupted restoring with non-empty folder must not silently discard edits"
        );
        let resumed_restoring = pc_a
            .takeover_project(
                "test",
                sync::TakeoverParams {
                    project_key: "project-alpha".to_owned(),
                    local_path: None,
                    delete_extra: true,
                    save_local_backup_first: false,
                    discard_local_changes: true,
                    force_while_remote_active: false,
                    dry_run: false,
                    confirm_project_key: Some("project-alpha".to_owned()),
                    expected_source_snapshot_id: None,
                    expected_canonical_path: None,
                },
            )
            .expect("re-running interrupted restoring with explicit confirmation must converge");
        assert_eq!(
            resumed_restoring.safety_backup_snapshot_id.as_deref(),
            Some(safety_id.as_str()),
            "resumed restoring must preserve prior safety_backup_snapshot_id"
        );
        let latest_id_after_resume = resumed_restoring
            .claimed_snapshot_id
            .clone()
            .expect("claimed snapshot");

        // 6b. Interrupted `claiming` on the same (source_id, safe_dest) skips
        // restore so post-restore edits are preserved and published in `handoff:claimed`.
        std::fs::write(
            proj_a.join("Assets").join("post-restore-edit.txt"),
            "must-survive-claim-resume",
        )
        .unwrap();
        {
            let db = HistoryDb::open(&pc_a.db_path).unwrap();
            db.set_handoff_state(
                "project-alpha",
                "claiming",
                Some(&latest_id_after_resume),
                Some(&proj_a_canon),
                None,
            )
            .unwrap();
        }
        pc_a.takeover_project(
            "test",
            sync::TakeoverParams {
                project_key: "project-alpha".to_owned(),
                local_path: None,
                delete_extra: true,
                save_local_backup_first: false,
                discard_local_changes: false,
                force_while_remote_active: false,
                dry_run: false,
                confirm_project_key: Some("project-alpha".to_owned()),
                expected_source_snapshot_id: None,
                expected_canonical_path: None,
            },
        )
        .expect("resuming claiming must succeed without overwriting post-restore edit");
        assert_eq!(
            std::fs::read_to_string(proj_a.join("Assets").join("post-restore-edit.txt")).unwrap(),
            "must-survive-claim-resume",
            "post-restore edit must survive claiming resume"
        );
        let recovered = pc_a
            .get_project_sync_status("test", Some("project-alpha"), false, false)
            .unwrap();
        assert_eq!(
            recovered.projects[0].state,
            sync::ProjectSyncStateKind::LocalLatest
        );
        assert_eq!(recovered.projects[0].handoff_status, "idle");
        std::thread::sleep(Duration::from_millis(50));

        // 7. Pre-backup remote check flips a stale `active` PC to `standby` instead of overwriting.
        pc_b.update_config(|cfg| {
            cfg.find_target_by_project_key_mut("project-alpha")
                .unwrap()
                .writer_state = WriterState::Active;
            Ok(())
        })
        .unwrap();
        let outcome = pc_b.execute_scheduled_backup("scheduler");
        assert_eq!(outcome, ScheduledBackupOutcome::SkippedNoActiveTargets);
        assert_eq!(
            pc_b.get_config()
                .find_target_by_project_key("project-alpha")
                .unwrap()
                .writer_state,
            WriterState::Standby
        );

        // 8. Legacy composite snapshot (untagged, 3 paths) and nested `.git` legacy rejection.
        let legacy_a = root_a.join("LegacyWorld");
        let sibling_a = root_a.join("SiblingWorld");
        let legacy_git_a = root_a.join("LegacyGitProj");
        std::fs::create_dir_all(&legacy_a).unwrap();
        std::fs::create_dir_all(&sibling_a).unwrap();
        // Nested vendor/Plugin/.git inside LegacyGitProj must also be detected and rejected.
        std::fs::create_dir_all(legacy_git_a.join("vendor").join("Plugin").join(".git")).unwrap();
        std::fs::write(legacy_a.join("world.txt"), "legacy-content").unwrap();
        std::fs::write(sibling_a.join("other.txt"), "do-not-restore").unwrap();
        std::fs::write(
            legacy_git_a
                .join("vendor")
                .join("Plugin")
                .join(".git")
                .join("HEAD"),
            "ref: refs/heads/main\n",
        )
        .unwrap();
        std::fs::write(legacy_git_a.join("README.md"), "nested-git-repo").unwrap();

        let mut legacy_cmd = std::process::Command::new(&restic_bin);
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            legacy_cmd.creation_flags(0x0800_0000);
        }
        let out = legacy_cmd
            .env("RESTIC_REPOSITORY", &repo_dir)
            .env("RESTIC_PASSWORD", "test-repo-pass-123")
            .args([
                "backup",
                "--host",
                "pc-a",
                &legacy_a.to_string_lossy(),
                &sibling_a.to_string_lossy(),
                &legacy_git_a.to_string_lossy(),
            ])
            .output()
            .expect("spawn legacy composite backup");
        assert!(
            out.status.success(),
            "legacy composite backup failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        std::thread::sleep(Duration::from_millis(50));

        // Legacy subtree containing nested `.git` must be rejected before touching local disk.
        let legacy_git_b = root_b.join("LegacyGitProj");
        assert!(
            pc_b.takeover_project(
                "test",
                sync::TakeoverParams {
                    project_key: "legacygitproj".to_owned(),
                    local_path: Some(legacy_git_b.to_string_lossy().into_owned()),
                    delete_extra: true,
                    save_local_backup_first: true,
                    discard_local_changes: false,
                    force_while_remote_active: true,
                    dry_run: false,
                    confirm_project_key: Some("legacygitproj".to_owned()),
                    expected_source_snapshot_id: None,
                    expected_canonical_path: None,
                },
            )
            .is_err()
        );
        assert!(!legacy_git_b.exists());

        let legacy_b = root_b.join("LegacyWorld");
        let legacy_take = pc_b
            .takeover_project(
                "test",
                sync::TakeoverParams {
                    project_key: "legacyworld".to_owned(),
                    local_path: Some(legacy_b.to_string_lossy().into_owned()),
                    delete_extra: true,
                    save_local_backup_first: true,
                    discard_local_changes: false,
                    force_while_remote_active: true,
                    dry_run: false,
                    confirm_project_key: Some("legacyworld".to_owned()),
                    expected_source_snapshot_id: None,
                    expected_canonical_path: None,
                },
            )
            .expect("takeover from legacy composite snapshot");
        assert!(legacy_take.is_legacy_composite);
        assert_eq!(
            std::fs::read_to_string(legacy_b.join("world.txt")).unwrap(),
            "legacy-content"
        );
        assert!(!legacy_b.join("other.txt").exists());
        assert!(!legacy_b.join("SiblingWorld").exists());

        // 9. Multi-PC forget_prune dry-run preview & slot-safe real prune:
        // Verify with keep_last=1 even when a safety-backup is present that BOTH the safety-backup
        // AND the newest effective snapshot survive, plus legacy composite survives!
        let dry_prune_raw = pc_a.forget_prune("test", true).expect("dry-run forget");
        let dry_prune: Value = serde_json::from_str(&dry_prune_raw).unwrap();
        assert_eq!(dry_prune["dryRun"], true);
        assert!(dry_prune["totalRemove"].as_u64().unwrap_or(0) >= 1);
        assert_eq!(dry_prune["protectedSafetyBackups"], 1);

        pc_a.forget_prune("test", false)
            .expect("multi-pc forget --prune");
        let remaining = runner::list_snapshots(
            &pc_a.get_config(),
            &pc_a.load_secrets().unwrap(),
            None,
            None,
        )
        .expect("list snapshots after forget_prune");

        let proj_snaps: Vec<&crate::restic::types::ResticSnapshot> = remaining
            .iter()
            .filter(|s| s.project_key() == Some("project-alpha"))
            .collect();
        assert!(
            proj_snaps.iter().any(|s| s.id == safety_id),
            "handoff:safety-backup snapshot must survive forget --prune"
        );
        let non_safety_proj = proj_snaps.iter().filter(|s| !s.is_safety_backup()).count();
        assert_eq!(
            non_safety_proj, 2,
            "shared project:project-alpha must be pruned across both PCs with keep_last=2"
        );
        assert!(
            remaining
                .iter()
                .any(|s| s.project_key().is_none() && s.paths.len() == 3),
            "legacy composite snapshot must survive forget --prune"
        );

        // Setting keep_last=1 with a newer safety-backup keeps both the newest effective snapshot
        // and the newest safety-backup after forget --prune.
        pc_a.update_config(|cfg| {
            cfg.retention.keep_last = 1;
            Ok(())
        })
        .unwrap();
        let newest_safety_tags = vec![TAG_HANDOFF_SAFETY_BACKUP.to_owned()];
        let target_a = pc_a
            .get_config()
            .find_target_by_project_key("project-alpha")
            .unwrap()
            .clone();
        let (newest_safety, _) = runner::run_backup_target(
            &pc_a.get_config(),
            &pc_a.load_secrets().unwrap(),
            &target_a,
            &BackupTargetOptions {
                dry_run: false,
                extra_tags: &[],
                internal_tags: &newest_safety_tags,
                suppress_project_tag: false,
                retry_lock: RETRY_LOCK_MANUAL_OR_HANDOFF,
                parent_snapshot: None,
                progress_label: None,
            },
            &pc_a.progress,
            &pc_a.cancel_flag,
        )
        .expect("create newest safety backup");
        let newest_safety_id = newest_safety.snapshot_id.unwrap();
        pc_a.forget_prune("test", false)
            .expect("forget --prune with keep_last=1 and newest safety-backup");
        let after_keep1 = runner::list_snapshots(
            &pc_a.get_config(),
            &pc_a.load_secrets().unwrap(),
            None,
            None,
        )
        .expect("list snapshots after keep_last=1 prune");
        let proj_after1: Vec<&crate::restic::types::ResticSnapshot> = after_keep1
            .iter()
            .filter(|s| s.project_key() == Some("project-alpha"))
            .collect();
        assert!(proj_after1.iter().any(|s| s.id == safety_id));
        assert!(proj_after1.iter().any(|s| s.id == newest_safety_id));
        assert_eq!(
            proj_after1.iter().filter(|s| !s.is_safety_backup()).count(),
            1,
            "newest effective snapshot MUST NOT be evicted by a newer safety-backup"
        );

        // Manual deletion of safety-backup snapshot via forget_snapshot.
        pc_a.forget_snapshot("test", &safety_id)
            .expect("manually delete safety-backup snapshot");
        let after_manual = runner::list_snapshots(
            &pc_a.get_config(),
            &pc_a.load_secrets().unwrap(),
            None,
            None,
        )
        .expect("list snapshots after forget_snapshot");
        assert!(
            !after_manual.iter().any(|s| s.id == safety_id),
            "manually deleted safety-backup must be gone"
        );

        // 10. Safe unlock (without --remove-all) succeeds.
        pc_a.unlock_repository("test").expect("safe unlock");

        // 11. Post-tag concurrent claim detection catches another PC's claim
        // even with an earlier wall clock, and busy `job_mutex` does not consume today's
        // maintenance date stamp.
        let mut initial_ids = std::collections::BTreeSet::new();
        initial_ids.insert("snap-released-1".to_owned());
        let snaps_with_race = vec![
            crate::restic::types::ResticSnapshot {
                id: "snap-released-1".to_owned(),
                short_id: "snap-rel".to_owned(),
                original: None,
                time: "2026-11-26T05:00:00Z".to_owned(),
                hostname: "pc-a".to_owned(),
                username: "u".to_owned(),
                paths: vec![r"D:\Proj".to_owned()],
                tags: vec![
                    "project:project-alpha".to_owned(),
                    "handoff:released".to_owned(),
                ],
            },
            crate::restic::types::ResticSnapshot {
                id: "snap-other-pc-claim".to_owned(),
                short_id: "snap-oth".to_owned(),
                original: None,
                // Skewed earlier clock on the competing PC still gets caught!
                time: "2026-11-26T04:59:50Z".to_owned(),
                hostname: "pc-c".to_owned(),
                username: "u".to_owned(),
                paths: vec![r"F:\Proj".to_owned()],
                tags: vec![
                    "project:project-alpha".to_owned(),
                    "handoff:claimed".to_owned(),
                ],
            },
            crate::restic::types::ResticSnapshot {
                id: "snap-my-claim".to_owned(),
                short_id: "snap-my".to_owned(),
                original: None,
                time: "2026-11-26T05:01:00Z".to_owned(),
                hostname: "pc-b".to_owned(),
                username: "u".to_owned(),
                paths: vec![r"E:\Proj".to_owned()],
                tags: vec![
                    "project:project-alpha".to_owned(),
                    "handoff:claimed".to_owned(),
                ],
            },
        ];
        assert!(matches!(
            find_concurrent_remote_claim_in_snapshots(
                &snaps_with_race,
                "project-alpha",
                "pc-b",
                "snap-released-1",
                &initial_ids,
                Some("snap-my-claim"),
            ),
            ConcurrentClaimCheck::ClaimedByOther(_)
        ));
        // Even if PC-C's skewed-clock claim was already in `initial_ids`, `handoff:from:<source>`
        // still catches the dual takeover!
        let mut initial_with_skewed_claim = initial_ids.clone();
        initial_with_skewed_claim.insert("snap-other-pc-claim".to_owned());
        let mut snaps_with_from_tag = snaps_with_race;
        snaps_with_from_tag[1]
            .tags
            .push("handoff:from:snap-released-1".to_owned());
        assert!(matches!(
            find_concurrent_remote_claim_in_snapshots(
                &snaps_with_from_tag,
                "project-alpha",
                "pc-b",
                "snap-released-1",
                &initial_with_skewed_claim,
                Some("snap-my-claim"),
            ),
            ConcurrentClaimCheck::ClaimedByOther(_)
        ));

        let db_a = HistoryDb::open(&pc_a.db_path).unwrap();
        let _busy = pc_a.job_mutex.lock().unwrap();
        assert!(
            !pc_a.forget_prune_scheduled_for_date(&db_a, "2026-11-26"),
            "busy job_mutex must skip maintenance without consuming today's slot"
        );
        assert_eq!(db_a.last_auto_maintenance_date().unwrap(), None);
        drop(_busy);

        // 12. Real-Restic verification of streaming `list_snapshot_files` with `--` and of
        // disabled `agent.allowPrune` preventing post-backup pruning during MCP `run_backup`.
        let one_entry = pc_a
            .handle_operation(
                "mcp",
                "list_snapshot_files",
                &json!({ "snapshotId": "latest", "maxEntries": 1 }),
            )
            .expect("streaming ls with cap=1");
        assert_eq!(one_entry["entries"].as_array().unwrap().len(), 1);

        // Passing a `-`-prefixed prefix must not be interpreted as a Restic option (e.g.
        // `--password-command`); Restic receives it strictly as a positional path filter after `--`
        // and rejects it as a non-absolute path filter without executing any command.
        let dash_err = pc_a
            .handle_operation(
                "mcp",
                "list_snapshot_files",
                &json!({
                    "snapshotId": "latest",
                    "prefix": "--password-command=calc.exe",
                    "maxEntries": 5
                }),
            )
            .unwrap_err();
        assert!(
            dash_err
                .to_string()
                .contains("All path filters must be absolute"),
            "expected Restic positional path-filter error after `--`, got: {dash_err}"
        );

        // Primary PC with `autoPruneAfterBackup: true`, `maintenanceAt: null`, `keepLast: 1`, and
        // `agent.allowPrune: false`: an MCP `run_backup` must NOT prune older snapshots, while an
        // operator-triggered ("scheduler") backup still prunes them.
        pc_a.update_config(|cfg| {
            cfg.schedule.auto_prune_after_backup = true;
            cfg.schedule.maintenance_at = None;
            cfg.retention.keep_last = 1;
            cfg.agent.allow_prune = false;
            Ok(())
        })
        .unwrap();
        let before_mcp_snaps = runner::list_snapshots(
            &pc_a.get_config(),
            &pc_a.load_secrets().unwrap(),
            None,
            None,
        )
        .expect("list snapshots before mcp backup");
        std::fs::write(proj_a.join("Assets").join("scene.txt"), "pc-a-v3-mcp").unwrap();
        pc_a.handle_operation(
            "mcp",
            "run_backup",
            &json!({ "projectKeys": ["project-alpha"] }),
        )
        .expect("mcp backup with allowPrune=false");
        let after_mcp_snaps = runner::list_snapshots(
            &pc_a.get_config(),
            &pc_a.load_secrets().unwrap(),
            None,
            None,
        )
        .expect("list snapshots after mcp backup");
        assert_eq!(
            after_mcp_snaps.len(),
            before_mcp_snaps.len() + 1,
            "MCP backup must not prune snapshots while agent.allowPrune is false"
        );
    }

    #[test]
    fn forget_prune_mcp_enforces_explicit_dual_consent_and_agent_policy() {
        let (_dir, ctrl) = test_controller();

        // `agent.allowPrune` is false by default; direct `forget_prune` is rejected immediately.
        assert!(!ctrl.get_config().agent.allow_prune);
        let default_err = ctrl
            .handle_operation("mcp", "forget_prune", &json!({ "dryRun": true }))
            .unwrap_err();
        assert!(default_err.to_string().contains("allowPrune"));

        // Even with `agent.allowPrune: true`, missing, malformed, and conflicting confirmations
        // must all be rejected before touching Restic.
        ctrl.update_config(|cfg| {
            cfg.agent.allow_prune = true;
            Ok(())
        })
        .unwrap();
        for bad_args in [
            json!({ "dryRun": false }),
            json!({ "dryRun": false, "confirmPrune": false }),
            json!({ "confirmPrune": true }),
            json!({ "dryRun": true, "confirmPrune": true }),
            json!({ "dryRun": "false", "confirmPrune": true }),
            json!({ "dryRun": false, "confirmPrune": "true" }),
            json!({ "dryRun": true, "unexpected": 1 }),
        ] {
            let err = ctrl
                .handle_operation("mcp", "forget_prune", &bad_args)
                .unwrap_err();
            assert!(
                matches!(err, AppError::InvalidArgument(_))
                    && !err.to_string().contains("allowPrune"),
                "expected confirmation/argument InvalidArgument for {bad_args}, got: {err}"
            );
        }

        // With `agent.allowPrune: false`, direct `forget_prune` and enabling post-backup pruning
        // via `update_schedule` are both rejected.
        ctrl.update_config(|cfg| {
            cfg.agent.allow_prune = false;
            cfg.schedule.auto_prune_after_backup = false;
            Ok(())
        })
        .unwrap();

        let err = ctrl
            .handle_operation("mcp", "forget_prune", &json!({ "dryRun": true }))
            .unwrap_err();
        assert!(err.to_string().contains("allowPrune"));

        let err = ctrl
            .handle_operation(
                "mcp",
                "update_schedule",
                &json!({ "autoPruneAfterBackup": true }),
            )
            .unwrap_err();
        assert!(err.to_string().contains("allowPrune"));
        assert!(!ctrl.get_config().schedule.auto_prune_after_backup);

        // Disabling `autoPruneAfterBackup` via MCP remains allowed even when `allowPrune` is false.
        ctrl.update_config(|cfg| {
            cfg.schedule.auto_prune_after_backup = true;
            cfg.schedule.maintenance_at = Some("05:00".to_owned());
            Ok(())
        })
        .unwrap();
        // Clearing `maintenanceAt` while `autoPruneAfterBackup` is true would activate legacy
        // post-backup pruning, so MCP must reject it when `allowPrune` is false.
        let err = ctrl
            .handle_operation("mcp", "update_schedule", &json!({ "maintenanceAt": null }))
            .unwrap_err();
        assert!(err.to_string().contains("allowPrune"));
        assert_eq!(
            ctrl.get_config().schedule.maintenance_at.as_deref(),
            Some("05:00")
        );

        ctrl.handle_operation(
            "mcp",
            "update_schedule",
            &json!({ "autoPruneAfterBackup": false }),
        )
        .expect("turning off autoPruneAfterBackup is always safe");
        assert!(!ctrl.get_config().schedule.auto_prune_after_backup);
    }

    #[test]
    fn mcp_prune_policy_guards_retention_and_maintenance_and_validates_retention_inputs() {
        let (_dir, ctrl) = test_controller();

        // 1. Default policy: restore is enabled (isolated targetDir), prune/delete/handoff are off.
        let initial = ctrl.get_config();
        assert!(initial.agent.allow_restore);
        assert!(!initial.agent.allow_prune);
        assert!(!initial.agent.allow_delete_storage);
        assert!(!initial.agent.allow_handoff);
        let initial_retention = initial.retention.clone();

        // 2. With `agent.allowPrune: false`, MCP cannot change retention policy (decrease,
        //    increase, or empty patch) and config remains untouched.
        for patch in [
            json!({ "keepLast": 1 }),
            json!({ "keepLast": 20 }),
            json!({ "keepDaily": 0, "keepWeekly": 0, "keepMonthly": 0 }),
            json!({}),
        ] {
            let err = ctrl
                .handle_operation("mcp", "update_retention_policy", &patch)
                .unwrap_err();
            assert!(
                matches!(err, AppError::InvalidArgument(_))
                    && err.to_string().contains("allowPrune"),
                "expected allowPrune rejection for {patch}, got: {err}"
            );
        }
        assert_eq!(ctrl.get_config().retention, initial_retention);

        // Even from all-zero retention (delete disabled), increasing to 1 via MCP is rejected.
        ctrl.update_config(|cfg| {
            cfg.retention.keep_last = 0;
            cfg.retention.keep_daily = 0;
            cfg.retention.keep_weekly = 0;
            cfg.retention.keep_monthly = 0;
            Ok(())
        })
        .unwrap();
        let err = ctrl
            .handle_operation("mcp", "update_retention_policy", &json!({ "keepLast": 1 }))
            .unwrap_err();
        assert!(err.to_string().contains("allowPrune"));
        assert_eq!(ctrl.get_config().retention.keep_last, 0);

        // Non-MCP callers (e.g. GUI) remain unaffected by `agent.allowPrune`.
        ctrl.handle_operation("gui", "update_retention_policy", &json!({ "keepLast": 10 }))
            .expect("gui update_retention_policy");
        assert_eq!(ctrl.get_config().retention.keep_last, 10);

        // 3. With `agent.allowPrune: false`, MCP cannot set or change `maintenanceAt`.
        let err = ctrl
            .handle_operation(
                "mcp",
                "update_schedule",
                &json!({ "maintenanceAt": "05:00" }),
            )
            .unwrap_err();
        assert!(err.to_string().contains("allowPrune"));
        assert!(ctrl.get_config().schedule.maintenance_at.is_none());

        // Non-MCP can set maintenanceAt; MCP still cannot change it to another time.
        ctrl.handle_operation(
            "gui",
            "update_schedule",
            &json!({ "maintenanceAt": "05:00" }),
        )
        .expect("gui set maintenanceAt");
        assert_eq!(
            ctrl.get_config().schedule.maintenance_at.as_deref(),
            Some("05:00")
        );

        let err = ctrl
            .handle_operation(
                "mcp",
                "update_schedule",
                &json!({ "maintenanceAt": "06:00" }),
            )
            .unwrap_err();
        assert!(err.to_string().contains("allowPrune"));
        assert_eq!(
            ctrl.get_config().schedule.maintenance_at.as_deref(),
            Some("05:00")
        );

        // Sending the same normalized time is a no-op and succeeds.
        ctrl.handle_operation(
            "mcp",
            "update_schedule",
            &json!({ "maintenanceAt": "5:00" }),
        )
        .expect("no-op maintenanceAt update");
        assert_eq!(
            ctrl.get_config().schedule.maintenance_at.as_deref(),
            Some("05:00")
        );

        // Clearing maintenanceAt while `autoPruneAfterBackup` is false is safe and allowed.
        ctrl.handle_operation("mcp", "update_schedule", &json!({ "maintenanceAt": null }))
            .expect("clearing maintenanceAt when autoPruneAfterBackup=false");
        assert!(ctrl.get_config().schedule.maintenance_at.is_none());

        // 4. `update_retention_policy` strictly rejects unknown keys, wrong types, negative
        //    values, out-of-range values, and u32-overflowing integers (both when allowPrune is
        //    false and when it is true).
        let bad_payloads = [
            json!(null),
            json!(123),
            json!([]),
            json!({ "keepLast": 10, "unknown": 1 }),
            json!({ "keepLast": "10" }),
            json!({ "keepDaily": 1.5 }),
            json!({ "keepWeekly": 1.0 }),
            json!({ "keepMonthly": true }),
            json!({ "keepLast": null }),
            json!({ "keepLast": -1 }),
            json!({ "keepLast": MAX_KEEP_LAST + 1 }),
            json!({ "keepDaily": MAX_KEEP_DAILY + 1 }),
            json!({ "keepWeekly": MAX_KEEP_WEEKLY + 1 }),
            json!({ "keepMonthly": MAX_KEEP_MONTHLY + 1 }),
            json!({ "keepLast": 1_u64 << 32 }),
            json!({ "keepLast": (1_u64 << 32) + 1 }),
            json!({ "keepLast": u64::MAX }),
        ];

        for allow_prune in [false, true] {
            ctrl.update_config(|cfg| {
                cfg.agent.allow_prune = allow_prune;
                Ok(())
            })
            .unwrap();
            let before = ctrl.get_config().retention;
            for bad in &bad_payloads {
                let err = ctrl
                    .handle_operation("mcp", "update_retention_policy", bad)
                    .unwrap_err();
                assert!(
                    matches!(err, AppError::InvalidArgument(_))
                        && !err.to_string().contains("allowPrune"),
                    "expected validation error (allowPrune={allow_prune}) for {bad}, got: {err}"
                );
            }
            assert_eq!(ctrl.get_config().retention, before);
        }

        // 5. With `agent.allowPrune: true`, valid retention boundaries and `maintenanceAt`
        //    updates succeed.
        let max_res = ctrl
            .handle_operation(
                "mcp",
                "update_retention_policy",
                &json!({
                    "keepLast": MAX_KEEP_LAST,
                    "keepDaily": MAX_KEEP_DAILY,
                    "keepWeekly": MAX_KEEP_WEEKLY,
                    "keepMonthly": MAX_KEEP_MONTHLY
                }),
            )
            .expect("valid max retention bounds");
        assert_eq!(max_res["keepLast"], MAX_KEEP_LAST);
        assert_eq!(max_res["keepDaily"], MAX_KEEP_DAILY);
        assert_eq!(max_res["keepWeekly"], MAX_KEEP_WEEKLY);
        assert_eq!(max_res["keepMonthly"], MAX_KEEP_MONTHLY);

        let zero_res = ctrl
            .handle_operation(
                "mcp",
                "update_retention_policy",
                &json!({
                    "keepLast": 0,
                    "keepDaily": 0,
                    "keepWeekly": 0,
                    "keepMonthly": 0
                }),
            )
            .expect("valid zero retention bounds");
        assert_eq!(zero_res["keepLast"], 0);

        let sched_set = ctrl
            .handle_operation(
                "mcp",
                "update_schedule",
                &json!({ "maintenanceAt": "04:30" }),
            )
            .expect("set maintenanceAt with allowPrune=true");
        assert_eq!(sched_set["maintenanceAt"], "04:30");

        let sched_changed = ctrl
            .handle_operation(
                "mcp",
                "update_schedule",
                &json!({ "maintenanceAt": "05:15" }),
            )
            .expect("change maintenanceAt with allowPrune=true");
        assert_eq!(sched_changed["maintenanceAt"], "05:15");
    }

    #[test]
    fn mcp_selection_validation_prevents_widening_restore_or_backup() {
        let (dir, ctrl) = test_controller();
        let target_dir = dir.path().join("TargetA");
        std::fs::create_dir_all(&target_dir).unwrap();
        ctrl.handle_operation(
            "mcp",
            "add_backup_target",
            &json!({ "path": target_dir.to_string_lossy() }),
        )
        .unwrap();

        // Existing destination with a sentinel file must stay completely untouched on invalid partial restore.
        let existing_dest = dir.path().join("existing-restore-dest");
        std::fs::create_dir_all(&existing_dest).unwrap();
        let sentinel = existing_dest.join("keep.txt");
        std::fs::write(&sentinel, "original-content").unwrap();

        for bad_includes in [
            json!([]),
            json!(["   "]),
            json!([123]),
            json!(["/valid", 42]),
            json!("/not-an-array"),
            json!(null),
        ] {
            let err = ctrl
                .handle_operation(
                    "mcp",
                    "restore_snapshot",
                    &json!({
                        "snapshotId": "latest",
                        "targetDir": existing_dest.to_string_lossy(),
                        "includePaths": bad_includes
                    }),
                )
                .unwrap_err();
            assert!(
                matches!(err, AppError::InvalidArgument(_)),
                "expected InvalidArgument for includePaths={bad_includes}, got: {err}"
            );
        }
        assert_eq!(
            std::fs::read_to_string(&sentinel).unwrap(),
            "original-content"
        );

        // Non-existent destination must not even be created when selection is invalid.
        let fresh_dest = dir.path().join("must-not-exist");
        assert!(
            ctrl.handle_operation(
                "mcp",
                "restore_snapshot",
                &json!({
                    "snapshotId": "latest",
                    "targetDir": fresh_dest.to_string_lossy(),
                    "includePaths": []
                }),
            )
            .is_err()
        );
        assert!(!fresh_dest.exists());

        // `run_backup` rejects empty, malformed, or unknown targetIds / projectKeys instead of
        // broadening to all active targets.
        for bad_backup in [
            json!({ "targetIds": [] }),
            json!({ "targetIds": "single-string" }),
            json!({ "targetIds": [123] }),
            json!({ "targetIds": ["  "] }),
            json!({ "targetIds": ["no-such-target"] }),
            json!({ "projectKeys": [] }),
            json!({ "projectKeys": "not-an-array" }),
            json!({ "projectKeys": [123] }),
        ] {
            let err = ctrl
                .handle_operation("mcp", "run_backup", &bad_backup)
                .unwrap_err();
            assert!(
                matches!(err, AppError::InvalidArgument(_)),
                "expected InvalidArgument for run_backup {bad_backup}, got: {err}"
            );
        }
    }

    #[test]
    fn mcp_rejects_restic_binary_override_and_flag_injection_snapshot_ids() {
        let (dir, ctrl) = test_controller();
        let existing_exe = std::env::current_exe().expect("current_exe");

        // Even an existing executable or null is rejected when called via MCP.
        for payload in [
            json!({ "resticBinaryPath": existing_exe.to_string_lossy() }),
            json!({ "resticBinaryPath": null }),
        ] {
            let err = ctrl
                .handle_operation("mcp", "update_storage_config", &payload)
                .unwrap_err();
            assert!(
                matches!(err, AppError::InvalidArgument(_))
                    && err.to_string().contains("resticBinaryPath"),
                "expected resticBinaryPath rejection for {payload}, got: {err}"
            );
        }
        assert!(ctrl.get_config().restic_binary_path.is_none());

        // Option-injection snapshot IDs are rejected across all snapshot-accepting MCP tools.
        let restore_dest = dir.path().join("no-create");
        for (op, args) in [
            (
                "list_snapshot_files",
                json!({ "snapshotId": "--password-command=calc.exe" }),
            ),
            (
                "get_size_breakdown",
                json!({ "snapshotId": "--password-command=calc.exe" }),
            ),
            (
                "diff_snapshots",
                json!({ "snapshotA": "latest", "snapshotB": "--password-command=calc.exe" }),
            ),
            (
                "restore_snapshot",
                json!({
                    "snapshotId": "--password-command=calc.exe",
                    "targetDir": restore_dest.to_string_lossy()
                }),
            ),
        ] {
            let err = ctrl.handle_operation("mcp", op, &args).unwrap_err();
            assert!(
                matches!(err, AppError::InvalidArgument(_)),
                "expected InvalidArgument for {op} {args}, got: {err}"
            );
        }
        assert!(!restore_dest.exists());
    }

    #[test]
    fn partial_secret_save_preserves_unreadable_vault_bytes() {
        if !crate::secrets::is_supported() {
            return;
        }
        let (_dir, ctrl) = test_controller();
        let vault_path = crate::secrets::credentials_path(&ctrl.base_dir);
        std::fs::create_dir_all(vault_path.parent().unwrap()).unwrap();
        let original_bytes = b"wrong-user-or-corrupt-dpapi-vault-bytes".to_vec();
        std::fs::write(&vault_path, &original_bytes).unwrap();

        let status = ctrl.status_report();
        assert!(status.secrets_readiness.vault_unreadable);
        assert!(!status.secrets_readiness.is_complete());
        assert!(status.secrets_error.is_some());

        // Attempting to save one field must fail and leave the original vault bytes untouched.
        let err = ctrl
            .update_secrets_partial(Some("new-access-key"), None, None)
            .unwrap_err();
        assert!(matches!(err, AppError::Secrets(_)));
        assert_eq!(
            std::fs::read(&vault_path).unwrap(),
            original_bytes,
            "unreadable credentials.dat bytes must be preserved on partial save"
        );
    }

    #[test]
    fn failed_config_update_does_not_mutate_live_state_on_validation_or_write_error() {
        let (_dir, ctrl) = test_controller();
        ctrl.update_config(|cfg| {
            cfg.agent.allow_prune = true;
            Ok(())
        })
        .unwrap();
        assert!(ctrl.get_config().schedule.enabled);
        assert!(!ctrl.get_config().schedule.auto_prune_after_backup);

        // 1. Validation error mid-update (invalid dailyAt after setting enabled=false & autoPrune=true).
        let err = ctrl
            .handle_operation(
                "mcp",
                "update_schedule",
                &json!({
                    "enabled": false,
                    "autoPruneAfterBackup": true,
                    "dailyAt": "99:99"
                }),
            )
            .unwrap_err();
        assert!(matches!(err, AppError::InvalidArgument(_)));
        assert!(ctrl.get_config().schedule.enabled);
        assert!(!ctrl.get_config().schedule.auto_prune_after_backup);

        // A later valid update must not carry over the rejected fields.
        ctrl.handle_operation("mcp", "update_schedule", &json!({ "intervalMinutes": 30 }))
            .expect("valid schedule update");
        let after = ctrl.reload_config_from_disk().unwrap();
        assert!(after.schedule.enabled);
        assert!(!after.schedule.auto_prune_after_backup);
        assert_eq!(after.schedule.interval_minutes, 30);

        // 2. Disk persistence failure: replace config_path with a directory so save_atomic fails.
        std::fs::remove_file(&ctrl.config_path).unwrap();
        std::fs::create_dir(&ctrl.config_path).unwrap();
        let write_err = ctrl
            .update_config(|cfg| {
                cfg.schedule.enabled = false;
                Ok(())
            })
            .unwrap_err();
        assert!(
            matches!(write_err, AppError::Io(_)),
            "expected I/O error, got: {write_err}"
        );
        assert!(
            ctrl.get_config().schedule.enabled,
            "live in-memory state must remain unchanged after persistence failure"
        );
    }

    #[test]
    fn concurrent_headless_controllers_preserve_target_and_schedule_updates() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cfg_path = dir.path().join("config.json");
        let target_a = dir.path().join("FolderA");
        let target_b = dir.path().join("FolderB");
        std::fs::create_dir_all(&target_a).unwrap();
        std::fs::create_dir_all(&target_b).unwrap();

        let ctrl_1 =
            SharedController::new(dir.path().to_path_buf(), cfg_path.clone()).expect("ctrl 1");
        ctrl_1
            .update_config(|cfg| {
                cfg.agent.allow_prune = true;
                Ok(())
            })
            .expect("enable allowPrune for retention test");
        let ctrl_2 =
            SharedController::new(dir.path().to_path_buf(), cfg_path.clone()).expect("ctrl 2");

        let barrier = Arc::new(std::sync::Barrier::new(2));
        let b1 = Arc::clone(&barrier);
        let b2 = Arc::clone(&barrier);
        let c1 = Arc::clone(&ctrl_1);
        let c2 = Arc::clone(&ctrl_2);
        let ta = target_a.clone();
        let tb = target_b.clone();

        let t1 = std::thread::spawn(move || {
            b1.wait();
            c1.handle_operation(
                "mcp",
                "add_backup_target",
                &json!({ "path": ta.to_string_lossy(), "label": "TargetA" }),
            )
            .expect("ctrl_1 add target A");
            c1.handle_operation(
                "mcp",
                "add_backup_target",
                &json!({ "path": tb.to_string_lossy(), "label": "TargetB" }),
            )
            .expect("ctrl_1 add target B");
        });

        let t2 = std::thread::spawn(move || {
            b2.wait();
            c2.handle_operation(
                "mcp",
                "update_schedule",
                &json!({ "intervalMinutes": 45, "dailyAt": ["06:30"] }),
            )
            .expect("ctrl_2 update schedule");
            c2.handle_operation("mcp", "update_retention_policy", &json!({ "keepLast": 12 }))
                .expect("ctrl_2 update retention");
        });

        t1.join().expect("thread 1");
        t2.join().expect("thread 2");

        let final_cfg = AppConfig::load_or_default(&cfg_path).expect("reload final config");
        assert_eq!(
            final_cfg.targets.len(),
            2,
            "both targets must survive concurrent schedule updates"
        );
        assert_eq!(final_cfg.schedule.interval_minutes, 45);
        assert_eq!(final_cfg.schedule.daily_at, vec!["06:30"]);
        assert_eq!(final_cfg.retention.keep_last, 12);
    }
}
