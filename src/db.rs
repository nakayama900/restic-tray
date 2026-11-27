//! Local SQLite database (WAL mode) for backup job history and MCP audit telemetry.

use std::path::Path;

use chrono::Utc;
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};

use crate::error::AppResult;
use crate::restic::types::{FolderBytes, ResticBackupSummary};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BackupRunRecord {
    pub id: i64,
    pub started_at: String,
    pub finished_at: String,
    pub trigger_source: String,
    pub target_summary: String,
    pub dry_run: bool,
    pub status: String,
    pub snapshot_id: Option<String>,
    pub files_new: u64,
    pub files_changed: u64,
    pub data_added_bytes: u64,
    pub total_bytes_processed: u64,
    pub duration_ms: u64,
    pub error_message: Option<String>,
    /// Folders that stored the most new data in the repository, largest first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub top_added: Vec<FolderBytes>,
    /// Files restic went through. Recorded from 0.3.1, so older runs have none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_files_processed: Option<u64>,
    /// Size and file count of the backed-up folders minus those of the previous successful
    /// real run of the same folders, so newly excluded or deleted data shows as negative.
    /// Set when loading, not stored.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bytes_delta: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub files_delta: Option<i64>,
    /// The exclude settings differ from that previous run, so files counted as new may be
    /// ones that were excluded before. Set when loading, not stored.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub excludes_changed: bool,
    #[serde(skip)]
    exclude_fingerprint: Option<String>,
}

/// Earlier rows read to find each listed run's previous run of the same folders.
const CHANGE_LOOKBACK: usize = 50;

/// Fills the change fields of `runs` (newest first) from the next older successful real run
/// with the same folders. Runs whose previous one is not in `runs` keep them empty.
fn fill_changes(runs: &mut [BackupRunRecord]) {
    for i in 0..runs.len() {
        let current = &runs[i];
        if current.status != "success" {
            continue;
        }
        let Some(previous) = runs[i + 1..].iter().find(|p| {
            !p.dry_run && p.status == "success" && p.target_summary == current.target_summary
        }) else {
            continue;
        };
        let bytes = current.total_bytes_processed as i64 - previous.total_bytes_processed as i64;
        let files = current
            .total_files_processed
            .zip(previous.total_files_processed)
            .map(|(now, before)| now as i64 - before as i64);
        let changed = matches!(
            (&current.exclude_fingerprint, &previous.exclude_fingerprint),
            (Some(now), Some(before)) if now != before
        );
        let run = &mut runs[i];
        run.bytes_delta = Some(bytes);
        run.files_delta = files;
        run.excludes_changed = changed;
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AuditLogRecord {
    pub id: i64,
    pub timestamp: String,
    pub actor: String,
    pub operation: String,
    pub detail: String,
}

/// Persistent per-project synchronization and handoff state cached in SQLite (`project_sync_state`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectSyncRow {
    pub project_key: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_synced_snapshot_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_synced_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_local_backup_snapshot_id: Option<String>,
    /// `"idle" | "restoring" | "claiming"`
    pub handoff_status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub handoff_source_snapshot_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub handoff_target_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub handoff_safety_snapshot_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_remote_seen_snapshot_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_remote_seen_host: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_remote_seen_at: Option<String>,
    #[serde(default)]
    pub last_remote_seen_released: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub last_remote_seen_paths: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_remote_checked_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_exclude_fingerprint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_remote_exclude_fingerprint: Option<String>,
    #[serde(default)]
    pub local_has_unbacked_changes: bool,
    #[serde(default)]
    pub safety_backup_count: u32,
}

impl ProjectSyncRow {
    pub fn new(project_key: impl Into<String>) -> Self {
        Self {
            project_key: project_key.into(),
            last_synced_snapshot_id: None,
            last_synced_at: None,
            last_local_backup_snapshot_id: None,
            handoff_status: "idle".to_owned(),
            handoff_source_snapshot_id: None,
            handoff_target_path: None,
            handoff_safety_snapshot_id: None,
            last_remote_seen_snapshot_id: None,
            last_remote_seen_host: None,
            last_remote_seen_at: None,
            last_remote_seen_released: false,
            last_remote_seen_paths: Vec::new(),
            last_remote_checked_at: None,
            last_exclude_fingerprint: None,
            last_remote_exclude_fingerprint: None,
            local_has_unbacked_changes: false,
            safety_backup_count: 0,
        }
    }
}

pub struct HistoryDb {
    conn: Connection,
}

impl HistoryDb {
    pub fn open(path: &Path) -> AppResult<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        let db = Self { conn };
        db.init_schema()?;
        Ok(db)
    }

    fn init_schema(&self) -> AppResult<()> {
        self.conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS backup_runs (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                started_at TEXT NOT NULL,
                finished_at TEXT NOT NULL,
                trigger_source TEXT NOT NULL,
                target_summary TEXT NOT NULL,
                dry_run INTEGER NOT NULL DEFAULT 0,
                status TEXT NOT NULL,
                snapshot_id TEXT,
                files_new INTEGER NOT NULL DEFAULT 0,
                files_changed INTEGER NOT NULL DEFAULT 0,
                data_added_bytes INTEGER NOT NULL DEFAULT 0,
                total_bytes_processed INTEGER NOT NULL DEFAULT 0,
                duration_ms INTEGER NOT NULL DEFAULT 0,
                error_message TEXT
            );
            CREATE TABLE IF NOT EXISTS audit_logs (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                timestamp TEXT NOT NULL,
                actor TEXT NOT NULL,
                operation TEXT NOT NULL,
                detail TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS project_sync_state (
                project_key TEXT PRIMARY KEY,
                last_synced_snapshot_id TEXT,
                last_synced_at TEXT,
                last_local_backup_snapshot_id TEXT,
                handoff_status TEXT NOT NULL DEFAULT 'idle',
                handoff_source_snapshot_id TEXT,
                handoff_target_path TEXT,
                handoff_safety_snapshot_id TEXT,
                last_remote_seen_snapshot_id TEXT,
                last_remote_seen_host TEXT,
                last_remote_seen_at TEXT,
                last_remote_seen_released INTEGER NOT NULL DEFAULT 0,
                last_remote_seen_paths TEXT,
                last_remote_checked_at TEXT,
                last_exclude_fingerprint TEXT,
                last_remote_exclude_fingerprint TEXT,
                local_has_unbacked_changes INTEGER NOT NULL DEFAULT 0,
                safety_backup_count INTEGER NOT NULL DEFAULT 0
            );
            CREATE TABLE IF NOT EXISTS sync_meta (
                key TEXT PRIMARY KEY,
                value TEXT NOT NULL
            );",
        )?;
        // Added after 0.2.1; `CREATE TABLE IF NOT EXISTS` leaves older databases without them.
        for (column, kind) in [
            ("top_added", "TEXT"),
            ("total_files_processed", "INTEGER"),
            ("exclude_fingerprint", "TEXT"),
        ] {
            let exists = self
                .conn
                .prepare("SELECT 1 FROM pragma_table_info('backup_runs') WHERE name = ?1")?
                .exists([column])?;
            if !exists {
                self.conn.execute(
                    &format!("ALTER TABLE backup_runs ADD COLUMN {column} {kind}"),
                    [],
                )?;
            }
        }
        for (column, kind) in [
            ("handoff_source_snapshot_id", "TEXT"),
            ("handoff_target_path", "TEXT"),
            ("handoff_safety_snapshot_id", "TEXT"),
        ] {
            let exists = self
                .conn
                .prepare("SELECT 1 FROM pragma_table_info('project_sync_state') WHERE name = ?1")?
                .exists([column])?;
            if !exists {
                self.conn.execute(
                    &format!("ALTER TABLE project_sync_state ADD COLUMN {column} {kind}"),
                    [],
                )?;
            }
        }
        Ok(())
    }

    pub fn record_backup_run(
        &self,
        started_at: &str,
        trigger_source: &str,
        target_summary: &str,
        dry_run: bool,
        result: Result<&(ResticBackupSummary, u64), &str>,
    ) -> AppResult<i64> {
        let finished_at = Utc::now().to_rfc3339();
        match result {
            Ok((summary, duration_ms)) => {
                // A saved snapshot is a success even when some files could not be read;
                // those are kept as a warning in `error_message`.
                let warning = (!summary.unreadable.is_empty()).then(|| {
                    format!(
                        "読み取れなかった項目 {} 件: {}",
                        summary.unreadable.len(),
                        summary.unreadable.join("; ")
                    )
                });
                let top_added = (!summary.top_added.is_empty())
                    .then(|| serde_json::to_string(&summary.top_added))
                    .transpose()?;
                self.conn.execute(
                    "INSERT INTO backup_runs (
                        started_at, finished_at, trigger_source, target_summary, dry_run,
                        status, snapshot_id, files_new, files_changed, data_added_bytes,
                        total_bytes_processed, duration_ms, error_message, top_added,
                        total_files_processed, exclude_fingerprint
                    ) VALUES (?1, ?2, ?3, ?4, ?5, 'success', ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13,
                        ?14, ?15)",
                    params![
                        started_at,
                        finished_at,
                        trigger_source,
                        target_summary,
                        i32::from(dry_run),
                        summary.snapshot_id,
                        summary.files_new as i64,
                        summary.files_changed as i64,
                        summary.data_added as i64,
                        summary.total_bytes_processed as i64,
                        *duration_ms as i64,
                        warning,
                        top_added,
                        summary.total_files_processed as i64,
                        summary.exclude_fingerprint,
                    ],
                )?;
            }
            Err(err_msg) => {
                self.conn.execute(
                    "INSERT INTO backup_runs (
                        started_at, finished_at, trigger_source, target_summary, dry_run,
                        status, snapshot_id, files_new, files_changed, data_added_bytes,
                        total_bytes_processed, duration_ms, error_message
                    ) VALUES (?1, ?2, ?3, ?4, ?5, 'failed', NULL, 0, 0, 0, 0, 0, ?6)",
                    params![
                        started_at,
                        finished_at,
                        trigger_source,
                        target_summary,
                        i32::from(dry_run),
                        err_msg,
                    ],
                )?;
            }
        }
        Ok(self.conn.last_insert_rowid())
    }

    pub fn record_backup_deferred(
        &self,
        started_at: &str,
        trigger_source: &str,
        target_summary: &str,
        dry_run: bool,
        reason: &str,
    ) -> AppResult<i64> {
        let finished_at = Utc::now().to_rfc3339();
        self.conn.execute(
            "INSERT INTO backup_runs (
                started_at, finished_at, trigger_source, target_summary, dry_run,
                status, snapshot_id, files_new, files_changed, data_added_bytes,
                total_bytes_processed, duration_ms, error_message
            ) VALUES (?1, ?2, ?3, ?4, ?5, 'deferred', NULL, 0, 0, 0, 0, 0, ?6)",
            params![
                started_at,
                finished_at,
                trigger_source,
                target_summary,
                i32::from(dry_run),
                reason,
            ],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Number of consecutive newest real backup runs with `status = 'deferred'`.
    pub fn consecutive_deferred_count(&self) -> AppResult<usize> {
        let mut stmt = self.conn.prepare(
            "SELECT status FROM backup_runs WHERE dry_run = 0 ORDER BY id DESC LIMIT 50",
        )?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
        let mut count = 0;
        for status in rows {
            if status? == "deferred" {
                count += 1;
            } else {
                break;
            }
        }
        Ok(count)
    }

    /// The newest audit entry whose exact operation name matches `operation`.
    pub fn last_audit_exact(&self, operation: &str) -> AppResult<Option<AuditLogRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, timestamp, actor, operation, detail
             FROM audit_logs
             WHERE operation = ?1
             ORDER BY id DESC
             LIMIT 1",
        )?;
        let mut rows = stmt.query_map(params![operation], |row| {
            Ok(AuditLogRecord {
                id: row.get(0)?,
                timestamp: row.get(1)?,
                actor: row.get(2)?,
                operation: row.get(3)?,
                detail: row.get(4)?,
            })
        })?;
        Ok(rows.next().transpose()?)
    }

    pub fn get_project_sync_state(&self, project_key: &str) -> AppResult<Option<ProjectSyncRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT project_key, last_synced_snapshot_id, last_synced_at,
                    last_local_backup_snapshot_id, handoff_status,
                    handoff_source_snapshot_id, handoff_target_path, handoff_safety_snapshot_id,
                    last_remote_seen_snapshot_id, last_remote_seen_host,
                    last_remote_seen_at, last_remote_seen_released,
                    last_remote_seen_paths, last_remote_checked_at,
                    last_exclude_fingerprint, last_remote_exclude_fingerprint,
                    local_has_unbacked_changes, safety_backup_count
             FROM project_sync_state
             WHERE project_key = ?1",
        )?;
        let mut rows = stmt.query_map(params![project_key], map_project_sync_row)?;
        Ok(rows.next().transpose()?)
    }

    pub fn list_project_sync_states(&self) -> AppResult<Vec<ProjectSyncRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT project_key, last_synced_snapshot_id, last_synced_at,
                    last_local_backup_snapshot_id, handoff_status,
                    handoff_source_snapshot_id, handoff_target_path, handoff_safety_snapshot_id,
                    last_remote_seen_snapshot_id, last_remote_seen_host,
                    last_remote_seen_at, last_remote_seen_released,
                    last_remote_seen_paths, last_remote_checked_at,
                    last_exclude_fingerprint, last_remote_exclude_fingerprint,
                    local_has_unbacked_changes, safety_backup_count
             FROM project_sync_state
             ORDER BY project_key ASC",
        )?;
        let rows = stmt.query_map([], map_project_sync_row)?;
        let mut out = Vec::new();
        for item in rows {
            out.push(item?);
        }
        Ok(out)
    }

    pub fn upsert_project_sync_state(&self, row: &ProjectSyncRow) -> AppResult<()> {
        let paths_json = (!row.last_remote_seen_paths.is_empty())
            .then(|| serde_json::to_string(&row.last_remote_seen_paths))
            .transpose()?;
        self.conn.execute(
            "INSERT INTO project_sync_state (
                project_key, last_synced_snapshot_id, last_synced_at,
                last_local_backup_snapshot_id, handoff_status,
                handoff_source_snapshot_id, handoff_target_path, handoff_safety_snapshot_id,
                last_remote_seen_snapshot_id, last_remote_seen_host,
                last_remote_seen_at, last_remote_seen_released,
                last_remote_seen_paths, last_remote_checked_at,
                last_exclude_fingerprint, last_remote_exclude_fingerprint,
                local_has_unbacked_changes, safety_backup_count
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18)
             ON CONFLICT(project_key) DO UPDATE SET
                last_synced_snapshot_id = excluded.last_synced_snapshot_id,
                last_synced_at = excluded.last_synced_at,
                last_local_backup_snapshot_id = excluded.last_local_backup_snapshot_id,
                handoff_status = excluded.handoff_status,
                handoff_source_snapshot_id = excluded.handoff_source_snapshot_id,
                handoff_target_path = excluded.handoff_target_path,
                handoff_safety_snapshot_id = excluded.handoff_safety_snapshot_id,
                last_remote_seen_snapshot_id = excluded.last_remote_seen_snapshot_id,
                last_remote_seen_host = excluded.last_remote_seen_host,
                last_remote_seen_at = excluded.last_remote_seen_at,
                last_remote_seen_released = excluded.last_remote_seen_released,
                last_remote_seen_paths = excluded.last_remote_seen_paths,
                last_remote_checked_at = excluded.last_remote_checked_at,
                last_exclude_fingerprint = excluded.last_exclude_fingerprint,
                last_remote_exclude_fingerprint = excluded.last_remote_exclude_fingerprint,
                local_has_unbacked_changes = excluded.local_has_unbacked_changes,
                safety_backup_count = excluded.safety_backup_count",
            params![
                row.project_key,
                row.last_synced_snapshot_id,
                row.last_synced_at,
                row.last_local_backup_snapshot_id,
                row.handoff_status,
                row.handoff_source_snapshot_id,
                row.handoff_target_path,
                row.handoff_safety_snapshot_id,
                row.last_remote_seen_snapshot_id,
                row.last_remote_seen_host,
                row.last_remote_seen_at,
                i32::from(row.last_remote_seen_released),
                paths_json,
                row.last_remote_checked_at,
                row.last_exclude_fingerprint,
                row.last_remote_exclude_fingerprint,
                i32::from(row.local_has_unbacked_changes),
                i64::from(row.safety_backup_count),
            ],
        )?;
        Ok(())
    }

    /// Column-scoped update for remote repository observations (`last_remote_*` and
    /// `safety_backup_count`), leaving local sync and active handoff columns untouched so
    /// background sync refreshes cannot race with an in-progress takeover.
    #[allow(clippy::too_many_arguments)]
    pub fn update_remote_snapshot_observation(
        &self,
        project_key: &str,
        snapshot_id: Option<&str>,
        host: Option<&str>,
        seen_at: Option<&str>,
        released: bool,
        paths: &[String],
        checked_at: &str,
        remote_exclude_fp: Option<&str>,
        safety_backup_count: u32,
    ) -> AppResult<()> {
        let paths_json = (!paths.is_empty())
            .then(|| serde_json::to_string(paths))
            .transpose()?;
        self.conn.execute(
            "INSERT INTO project_sync_state (
                project_key, handoff_status,
                last_remote_seen_snapshot_id, last_remote_seen_host,
                last_remote_seen_at, last_remote_seen_released,
                last_remote_seen_paths, last_remote_checked_at,
                last_remote_exclude_fingerprint, safety_backup_count
             ) VALUES (?1, 'idle', ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
             ON CONFLICT(project_key) DO UPDATE SET
                last_remote_seen_snapshot_id = excluded.last_remote_seen_snapshot_id,
                last_remote_seen_host = excluded.last_remote_seen_host,
                last_remote_seen_at = excluded.last_remote_seen_at,
                last_remote_seen_released = excluded.last_remote_seen_released,
                last_remote_seen_paths = excluded.last_remote_seen_paths,
                last_remote_checked_at = excluded.last_remote_checked_at,
                last_remote_exclude_fingerprint = excluded.last_remote_exclude_fingerprint,
                safety_backup_count = excluded.safety_backup_count",
            params![
                project_key,
                snapshot_id,
                host,
                seen_at,
                i32::from(released),
                paths_json,
                checked_at,
                remote_exclude_fp,
                i64::from(safety_backup_count),
            ],
        )?;
        Ok(())
    }

    /// Column-scoped update recording a completed local backup, release, or takeover claim.
    #[allow(clippy::too_many_arguments)]
    pub fn update_local_backup_sync(
        &self,
        project_key: &str,
        snapshot_id: &str,
        synced_at: &str,
        local_host: &str,
        released: bool,
        target_path: &str,
        exclude_fp: Option<&str>,
    ) -> AppResult<()> {
        let paths_json = serde_json::to_string(&[target_path])?;
        self.conn.execute(
            "INSERT INTO project_sync_state (
                project_key, last_synced_snapshot_id, last_synced_at,
                last_local_backup_snapshot_id, handoff_status,
                handoff_source_snapshot_id, handoff_target_path, handoff_safety_snapshot_id,
                last_remote_seen_snapshot_id, last_remote_seen_host,
                last_remote_seen_at, last_remote_seen_released,
                last_remote_seen_paths, last_remote_checked_at,
                last_exclude_fingerprint, last_remote_exclude_fingerprint,
                local_has_unbacked_changes
             ) VALUES (?1, ?2, ?3, ?2, 'idle', NULL, NULL, NULL, ?2, ?4, ?3, ?5, ?6, ?3, ?7, ?7, 0)
             ON CONFLICT(project_key) DO UPDATE SET
                last_synced_snapshot_id = excluded.last_synced_snapshot_id,
                last_synced_at = excluded.last_synced_at,
                last_local_backup_snapshot_id = excluded.last_local_backup_snapshot_id,
                handoff_status = 'idle',
                handoff_source_snapshot_id = NULL,
                handoff_target_path = NULL,
                handoff_safety_snapshot_id = NULL,
                last_remote_seen_snapshot_id = excluded.last_remote_seen_snapshot_id,
                last_remote_seen_host = excluded.last_remote_seen_host,
                last_remote_seen_at = excluded.last_remote_seen_at,
                last_remote_seen_released = excluded.last_remote_seen_released,
                last_remote_seen_paths = excluded.last_remote_seen_paths,
                last_remote_checked_at = excluded.last_remote_checked_at,
                last_exclude_fingerprint = excluded.last_exclude_fingerprint,
                last_remote_exclude_fingerprint = excluded.last_remote_exclude_fingerprint,
                local_has_unbacked_changes = 0",
            params![
                project_key,
                snapshot_id,
                synced_at,
                local_host,
                i32::from(released),
                paths_json,
                exclude_fp,
            ],
        )?;
        Ok(())
    }

    /// Column-scoped update for in-progress handoff checkpoint columns only.
    pub fn set_handoff_state(
        &self,
        project_key: &str,
        status: &str,
        source_snapshot_id: Option<&str>,
        target_path: Option<&str>,
        safety_snapshot_id: Option<&str>,
    ) -> AppResult<()> {
        self.conn.execute(
            "INSERT INTO project_sync_state (
                project_key, handoff_status,
                handoff_source_snapshot_id, handoff_target_path, handoff_safety_snapshot_id
             ) VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(project_key) DO UPDATE SET
                handoff_status = excluded.handoff_status,
                handoff_source_snapshot_id = excluded.handoff_source_snapshot_id,
                handoff_target_path = excluded.handoff_target_path,
                handoff_safety_snapshot_id = excluded.handoff_safety_snapshot_id",
            params![
                project_key,
                status,
                source_snapshot_id,
                target_path,
                safety_snapshot_id,
            ],
        )?;
        Ok(())
    }

    pub fn set_handoff_status(&self, project_key: &str, status: &str) -> AppResult<()> {
        if status == "idle" {
            self.set_handoff_state(project_key, "idle", None, None, None)
        } else {
            self.conn.execute(
                "INSERT INTO project_sync_state (project_key, handoff_status) VALUES (?1, ?2)
                 ON CONFLICT(project_key) DO UPDATE SET handoff_status = excluded.handoff_status",
                params![project_key, status],
            )?;
            Ok(())
        }
    }

    /// Column-scoped update for the cached local unbacked-changes flag.
    pub fn set_local_has_unbacked_changes(
        &self,
        project_key: &str,
        has_changes: bool,
    ) -> AppResult<()> {
        self.conn.execute(
            "INSERT INTO project_sync_state (project_key, handoff_status, local_has_unbacked_changes)
             VALUES (?1, 'idle', ?2)
             ON CONFLICT(project_key) DO UPDATE SET
                local_has_unbacked_changes = excluded.local_has_unbacked_changes",
            params![project_key, i32::from(has_changes)],
        )?;
        Ok(())
    }

    pub fn consecutive_scheduled_deferred_count(&self) -> AppResult<usize> {
        Ok(self
            .get_sync_meta("consecutive_scheduled_deferred")?
            .and_then(|s| s.parse::<usize>().ok())
            .unwrap_or(0))
    }

    pub fn set_consecutive_scheduled_deferred_count(&self, count: usize) -> AppResult<()> {
        self.set_sync_meta("consecutive_scheduled_deferred", &count.to_string())
    }

    pub fn last_auto_maintenance_date(&self) -> AppResult<Option<String>> {
        self.get_sync_meta("last_auto_maintenance_date")
    }

    pub fn set_last_auto_maintenance_date(&self, ymd: &str) -> AppResult<()> {
        self.set_sync_meta("last_auto_maintenance_date", ymd)
    }

    pub fn get_sync_meta(&self, key: &str) -> AppResult<Option<String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT value FROM sync_meta WHERE key = ?1")?;
        let mut rows = stmt.query_map(params![key], |row| row.get::<_, String>(0))?;
        Ok(rows.next().transpose()?)
    }

    pub fn set_sync_meta(&self, key: &str, value: &str) -> AppResult<()> {
        self.conn.execute(
            "INSERT INTO sync_meta (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    pub fn observed_remote_hosts(&self) -> AppResult<Vec<String>> {
        Ok(self
            .get_sync_meta("observed_remote_hosts")?
            .and_then(|json| serde_json::from_str::<Vec<String>>(&json).ok())
            .unwrap_or_default())
    }

    pub fn set_observed_remote_hosts(&self, hosts: &[String]) -> AppResult<()> {
        let json = serde_json::to_string(hosts)?;
        self.set_sync_meta("observed_remote_hosts", &json)
    }

    pub fn last_remote_checked_at(&self) -> AppResult<Option<String>> {
        self.get_sync_meta("last_remote_checked_at")
    }

    pub fn set_last_remote_checked_at(&self, ts: &str) -> AppResult<()> {
        self.set_sync_meta("last_remote_checked_at", ts)
    }

    pub fn record_audit(&self, actor: &str, operation: &str, detail: &str) -> AppResult<()> {
        let now = Utc::now().to_rfc3339();
        self.conn.execute(
            "INSERT INTO audit_logs (timestamp, actor, operation, detail) VALUES (?1, ?2, ?3, ?4)",
            params![now, actor, operation, detail],
        )?;
        Ok(())
    }

    pub fn recent_backup_runs(&self, limit: usize) -> AppResult<Vec<BackupRunRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, started_at, finished_at, trigger_source, target_summary, dry_run,
                    status, CASE WHEN dry_run = 0 THEN snapshot_id END, files_new,
                    files_changed, data_added_bytes,
                    total_bytes_processed, duration_ms, error_message, top_added,
                    total_files_processed, exclude_fingerprint
             FROM backup_runs
             ORDER BY id DESC
             LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![(limit + CHANGE_LOOKBACK) as i64], |row| {
            Ok(BackupRunRecord {
                id: row.get(0)?,
                started_at: row.get(1)?,
                finished_at: row.get(2)?,
                trigger_source: row.get(3)?,
                target_summary: row.get(4)?,
                dry_run: row.get::<_, i32>(5)? != 0,
                status: row.get(6)?,
                snapshot_id: row.get(7)?,
                files_new: row.get::<_, i64>(8)? as u64,
                files_changed: row.get::<_, i64>(9)? as u64,
                data_added_bytes: row.get::<_, i64>(10)? as u64,
                total_bytes_processed: row.get::<_, i64>(11)? as u64,
                duration_ms: row.get::<_, i64>(12)? as u64,
                error_message: row.get(13)?,
                top_added: row
                    .get::<_, Option<String>>(14)?
                    .and_then(|json| serde_json::from_str(&json).ok())
                    .unwrap_or_default(),
                total_files_processed: row.get::<_, Option<i64>>(15)?.map(|n| n as u64),
                bytes_delta: None,
                files_delta: None,
                excludes_changed: false,
                exclude_fingerprint: row.get(16)?,
            })
        })?;
        let mut out = Vec::new();
        for item in rows {
            out.push(item?);
        }
        fill_changes(&mut out);
        out.truncate(limit);
        Ok(out)
    }

    /// The newest audit entry whose operation is `operation` or `operation` + `_failed`.
    pub fn last_audit_of(&self, operation: &str) -> AppResult<Option<AuditLogRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, timestamp, actor, operation, detail
             FROM audit_logs
             WHERE operation IN (?1, ?1 || '_failed')
             ORDER BY id DESC
             LIMIT 1",
        )?;
        let mut rows = stmt.query_map(params![operation], |row| {
            Ok(AuditLogRecord {
                id: row.get(0)?,
                timestamp: row.get(1)?,
                actor: row.get(2)?,
                operation: row.get(3)?,
                detail: row.get(4)?,
            })
        })?;
        Ok(rows.next().transpose()?)
    }

    pub fn recent_audit_logs(&self, limit: usize) -> AppResult<Vec<AuditLogRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, timestamp, actor, operation, detail
             FROM audit_logs
             ORDER BY id DESC
             LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![limit as i64], |row| {
            Ok(AuditLogRecord {
                id: row.get(0)?,
                timestamp: row.get(1)?,
                actor: row.get(2)?,
                operation: row.get(3)?,
                detail: row.get(4)?,
            })
        })?;
        let mut out = Vec::new();
        for item in rows {
            out.push(item?);
        }
        Ok(out)
    }
}

fn map_project_sync_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ProjectSyncRow> {
    Ok(ProjectSyncRow {
        project_key: row.get(0)?,
        last_synced_snapshot_id: row.get(1)?,
        last_synced_at: row.get(2)?,
        last_local_backup_snapshot_id: row.get(3)?,
        handoff_status: row.get(4)?,
        handoff_source_snapshot_id: row.get(5)?,
        handoff_target_path: row.get(6)?,
        handoff_safety_snapshot_id: row.get(7)?,
        last_remote_seen_snapshot_id: row.get(8)?,
        last_remote_seen_host: row.get(9)?,
        last_remote_seen_at: row.get(10)?,
        last_remote_seen_released: row.get::<_, i32>(11)? != 0,
        last_remote_seen_paths: row
            .get::<_, Option<String>>(12)?
            .and_then(|json| serde_json::from_str(&json).ok())
            .unwrap_or_default(),
        last_remote_checked_at: row.get(13)?,
        last_exclude_fingerprint: row.get(14)?,
        last_remote_exclude_fingerprint: row.get(15)?,
        local_has_unbacked_changes: row.get::<_, i32>(16)? != 0,
        safety_backup_count: row.get::<_, i64>(17)?.max(0) as u32,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sqlite_records_backup_runs_and_audit_logs() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("history.db");
        let db = HistoryDb::open(&db_path).expect("open db");

        let summary = ResticBackupSummary {
            files_new: 5,
            files_changed: 1,
            data_added: 10240,
            total_bytes_processed: 51200,
            snapshot_id: Some("snap001".to_owned()),
            ..Default::default()
        };
        db.record_backup_run(
            "2026-11-07T03:00:00Z",
            "mcp",
            "Workspaces",
            false,
            Ok(&(summary, 420)),
        )
        .expect("record run");
        db.record_audit("mcp", "run_backup", "triggered backup for Workspaces")
            .expect("record audit");

        let runs = db.recent_backup_runs(10).expect("load runs");
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].snapshot_id.as_deref(), Some("snap001"));
        assert_eq!(runs[0].files_new, 5);

        let audits = db.recent_audit_logs(10).expect("load audits");
        assert_eq!(audits.len(), 1);
        assert_eq!(audits[0].actor, "mcp");
    }

    #[test]
    fn open_adds_top_added_to_a_database_from_an_older_version() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("history.db");
        {
            let old = Connection::open(&db_path).expect("old db");
            old.execute_batch(
                "CREATE TABLE backup_runs (
                    id INTEGER PRIMARY KEY AUTOINCREMENT, started_at TEXT NOT NULL,
                    finished_at TEXT NOT NULL, trigger_source TEXT NOT NULL,
                    target_summary TEXT NOT NULL, dry_run INTEGER NOT NULL DEFAULT 0,
                    status TEXT NOT NULL, snapshot_id TEXT,
                    files_new INTEGER NOT NULL DEFAULT 0, files_changed INTEGER NOT NULL DEFAULT 0,
                    data_added_bytes INTEGER NOT NULL DEFAULT 0,
                    total_bytes_processed INTEGER NOT NULL DEFAULT 0,
                    duration_ms INTEGER NOT NULL DEFAULT 0, error_message TEXT);
                INSERT INTO backup_runs (started_at, finished_at, trigger_source, target_summary, status)
                    VALUES ('2026-11-09T08:00:00Z', '2026-11-09T08:03:00Z', 'scheduler', 'old', 'success');",
            )
            .expect("old schema");
        }

        let db = HistoryDb::open(&db_path).expect("open migrates");
        let summary = ResticBackupSummary {
            snapshot_id: Some("snap003".to_owned()),
            top_added: vec![FolderBytes {
                path: r"D:\p\_Backups".to_owned(),
                bytes: 2048,
            }],
            ..Default::default()
        };
        db.record_backup_run(
            "2026-11-10T08:00:00Z",
            "gui",
            "new",
            false,
            Ok(&(summary, 1)),
        )
        .expect("record run");
        HistoryDb::open(&db_path).expect("reopen keeps the column");

        let runs = db.recent_backup_runs(10).expect("load runs");
        assert_eq!(runs[0].top_added[0].path, r"D:\p\_Backups");
        assert_eq!(runs[0].top_added[0].bytes, 2048);
        assert!(runs[1].top_added.is_empty());
    }

    #[test]
    fn recent_backup_runs_compares_each_run_with_the_previous_real_run_of_the_same_folders() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = HistoryDb::open(&dir.path().join("history.db")).expect("open db");
        let ok = |bytes: u64, files: u64, fingerprint: &str| ResticBackupSummary {
            total_bytes_processed: bytes,
            total_files_processed: files,
            exclude_fingerprint: Some(fingerprint.to_owned()),
            ..Default::default()
        };
        let at = "2026-11-10T00:00:00Z";
        db.record_backup_run(at, "scheduler", "A", false, Ok(&(ok(1000, 10, "x"), 1)))
            .unwrap();
        db.record_backup_run(at, "scheduler", "B", false, Ok(&(ok(50, 5, "x"), 1)))
            .unwrap();
        db.record_backup_run(at, "gui", "A", true, Ok(&(ok(900, 9, "x"), 1)))
            .unwrap();
        db.record_backup_run(at, "gui", "A", false, Err("restic failed"))
            .unwrap();
        db.record_backup_run(at, "mcp", "A", false, Ok(&(ok(700, 7, "y"), 1)))
            .unwrap();

        let runs = db.recent_backup_runs(10).unwrap();
        let change = |r: &BackupRunRecord| (r.bytes_delta, r.files_delta, r.excludes_changed);
        // Newest first: the excludes changed and data went down, measured from the first run.
        assert_eq!(change(&runs[0]), (Some(-300), Some(-3), true));
        assert_eq!(change(&runs[1]), (None, None, false));
        assert_eq!(change(&runs[2]), (Some(-100), Some(-1), false));
        assert_eq!(change(&runs[3]), (None, None, false));
        assert_eq!(change(&runs[4]), (None, None, false));
        assert_eq!(db.recent_backup_runs(1).unwrap()[0].bytes_delta, Some(-300));
    }

    #[test]
    fn recent_backup_runs_hides_the_snapshot_id_of_a_dry_run() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = HistoryDb::open(&dir.path().join("history.db")).expect("open db");
        let summary = ResticBackupSummary {
            snapshot_id: Some("never-saved".to_owned()),
            ..Default::default()
        };
        db.record_backup_run(
            "2026-11-11T00:00:00Z",
            "mcp",
            "docs",
            true,
            Ok(&(summary, 1)),
        )
        .expect("record run");

        assert_eq!(
            db.recent_backup_runs(1).expect("load runs")[0].snapshot_id,
            None
        );
    }

    #[test]
    fn record_backup_run_keeps_a_saved_snapshot_with_unreadable_files_as_success() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = HistoryDb::open(&dir.path().join("history.db")).expect("open db");
        let summary = ResticBackupSummary {
            snapshot_id: Some("snap002".to_owned()),
            unreadable: vec![r"D:\p\.pytest_cache".to_owned()],
            ..Default::default()
        };
        db.record_backup_run(
            "2026-11-07T08:34:25Z",
            "gui",
            "SampleProject",
            false,
            Ok(&(summary, 1)),
        )
        .expect("record run");

        let run = &db.recent_backup_runs(1).expect("load runs")[0];
        assert_eq!(run.status, "success");
        assert_eq!(run.snapshot_id.as_deref(), Some("snap002"));
        assert_eq!(
            run.error_message.as_deref(),
            Some(r"読み取れなかった項目 1 件: D:\p\.pytest_cache")
        );
    }

    #[test]
    fn deferred_runs_and_project_sync_state_persist() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = HistoryDb::open(&dir.path().join("history.db")).expect("open db");

        db.record_backup_deferred(
            "2026-11-26T05:00:00Z",
            "scheduler",
            "ProjectAlpha",
            false,
            "lock timeout",
        )
        .unwrap();
        db.record_backup_deferred(
            "2026-11-26T05:15:00Z",
            "scheduler",
            "ProjectAlpha",
            false,
            "lock timeout",
        )
        .unwrap();
        db.record_backup_deferred(
            "2026-11-26T05:30:00Z",
            "scheduler",
            "ProjectAlpha",
            false,
            "lock timeout",
        )
        .unwrap();
        assert_eq!(db.consecutive_deferred_count().unwrap(), 3);

        let mut state = ProjectSyncRow::new("project-alpha");
        state.last_synced_snapshot_id = Some("snap123".to_owned());
        state.last_remote_seen_host = Some("pc-a".to_owned());
        state.last_remote_seen_released = true;
        state.last_remote_seen_paths = vec![r"D:\Projects\ProjectAlpha".to_owned()];
        state.safety_backup_count = 2;
        db.upsert_project_sync_state(&state).unwrap();

        let loaded = db.get_project_sync_state("project-alpha").unwrap().unwrap();
        assert_eq!(loaded, state);

        db.set_handoff_status("project-alpha", "restoring").unwrap();
        assert_eq!(
            db.get_project_sync_state("project-alpha")
                .unwrap()
                .unwrap()
                .handoff_status,
            "restoring"
        );

        db.set_observed_remote_hosts(&["pc-a".to_owned()]).unwrap();
        assert_eq!(db.observed_remote_hosts().unwrap(), vec!["pc-a".to_owned()]);
    }
}
