//! Multi-PC project sync state classification, snapshot selection, and handoff types.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use chrono::DateTime;
use serde::{Deserialize, Serialize};

use crate::config::{AppConfig, BackupTarget, MaintenanceRole, WriterState, suggest_project_key};
use crate::db::{HistoryDb, ProjectSyncRow};
use crate::error::{AppError, AppResult};
use crate::fs_safety::{find_unity_lockfile, is_git_repository, snapshot_path_folder_name};
use crate::restic::excludes::exclude_fingerprint;
use crate::restic::types::ResticSnapshot;

/// Classification of a project's multi-PC sync state (see `docs/ARCHITECTURE.md`), including
/// explicit `standby` and `unchecked` distinctions so unverified or locally-released targets
/// never inflate `local_latest`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProjectSyncStateKind {
    LocalLatest,
    RemoteNewer,
    NotLocal,
    Diverged,
    LocalOnly,
    Standby,
    Unchecked,
}

/// Cached count summary embedded into `get_status` and the Overview page.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectSyncSummary {
    pub local_latest: usize,
    pub remote_newer: usize,
    pub diverged: usize,
    pub not_local: usize,
    pub local_only: usize,
    #[serde(default)]
    pub standby: usize,
    #[serde(default)]
    pub unchecked: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_checked_at: Option<String>,
}

/// Detailed sync status of one local target or remote-only shared project.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectSyncItem {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_key: Option<String>,
    pub state: ProjectSyncStateKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub local_target_id: Option<String>,
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub local_path: Option<String>,
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub writer_state: Option<WriterState>,
    pub handoff_status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latest_snapshot_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latest_short_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latest_host: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latest_time: Option<String>,
    pub handoff_ready: bool,
    pub safety_backup_count: u32,
    pub has_unbacked_local_changes: bool,
    pub exclude_fingerprint_mismatch: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub remote_paths: Vec<String>,
    pub is_git_repository: bool,
    pub unity_locked: bool,
    #[serde(default)]
    pub remote_checked: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub suggested_local_path: Option<String>,
}

/// Full project synchronization report returned by `get_project_sync_status` and cached for GUI.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectSyncReport {
    pub machine_name: String,
    pub maintenance_role: MaintenanceRole,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_checked_at: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub observed_remote_hosts: Vec<String>,
    pub summary: ProjectSyncSummary,
    pub projects: Vec<ProjectSyncItem>,
}

/// Parameters for `takeover_project`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TakeoverParams {
    pub project_key: String,
    pub local_path: Option<String>,
    pub dry_run: bool,
    pub save_local_backup_first: bool,
    pub discard_local_changes: bool,
    pub force_while_remote_active: bool,
    pub delete_extra: bool,
    pub confirm_project_key: Option<String>,
    /// Optional snapshot ID observed during GUI preview; if remote advanced since preview, abort.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_source_snapshot_id: Option<String>,
    /// Optional canonical destination path observed during GUI preview; if the path/junction now
    /// resolves elsewhere, abort before writing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_canonical_path: Option<String>,
}

/// Preview (`dry_run: true`) or execution result (`dry_run: false`) of `takeover_project`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TakeoverReport {
    pub dry_run: bool,
    pub project_key: String,
    pub source_snapshot_id: String,
    pub source_short_id: String,
    pub source_host: String,
    pub source_time: String,
    pub source_path: String,
    pub source_subpath: String,
    pub is_legacy_composite: bool,
    pub remote_released: bool,
    pub remote_possibly_active: bool,
    pub local_path: String,
    pub local_dir_exists: bool,
    pub has_unbacked_local_changes: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unbacked_changes_reason: Option<String>,
    pub was_interrupted: bool,
    pub exclude_fingerprint_mismatch: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub local_exclude_fingerprint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_exclude_fingerprint: Option<String>,
    pub delete_extra: bool,
    pub will_save_safety_backup: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub safety_backup_snapshot_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claimed_snapshot_id: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

/// Compares snapshot timestamps using RFC3339 instants, falling back to lexicographical order.
pub fn compare_snapshot_time(a: &ResticSnapshot, b: &ResticSnapshot) -> std::cmp::Ordering {
    match (
        DateTime::parse_from_rfc3339(a.time.trim()),
        DateTime::parse_from_rfc3339(b.time.trim()),
    ) {
        (Ok(ta), Ok(tb)) => ta.cmp(&tb).then_with(|| a.id.cmp(&b.id)),
        _ => a.time.cmp(&b.time).then_with(|| a.id.cmp(&b.id)),
    }
}

/// Picks the newest effective snapshot for `project_key`, excluding `handoff:safety-backup`
/// and unpublished `handoff:staging` snapshots.
pub fn select_latest_effective_snapshot<'a>(
    snaps: &'a [ResticSnapshot],
    project_key: &str,
) -> Option<&'a ResticSnapshot> {
    snaps
        .iter()
        .filter(|s| {
            s.project_key() == Some(project_key) && !s.is_safety_backup() && !s.is_handoff_staging()
        })
        .max_by(|a, b| compare_snapshot_time(a, b))
}

/// Outcome of matching a `project_key` / folder name against a snapshot's `paths`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SnapshotPathMatch<'a> {
    Matched(&'a str),
    NotFound,
    Ambiguous(Vec<&'a str>),
}

/// Fallback for projects that have not yet been backed up individually with `project:<key>`:
/// scans legacy snapshots (without any `project:*` tag, safety, or staging tags) newest-first.
/// Stops immediately with `Err` if the newest matching legacy snapshot has multiple ambiguous
/// paths for `project_key` rather than silently falling back to an older snapshot.
pub fn select_legacy_composite_candidate<'a>(
    snaps: &'a [ResticSnapshot],
    project_key: &str,
    local_folder_name: Option<&str>,
) -> AppResult<Option<(&'a ResticSnapshot, &'a str)>> {
    let mut legacy: Vec<&ResticSnapshot> = snaps
        .iter()
        .filter(|s| s.project_key().is_none() && !s.is_safety_backup() && !s.is_handoff_staging())
        .collect();
    legacy.sort_by(|a, b| compare_snapshot_time(b, a));
    for snap in legacy {
        match resolve_snapshot_source_path_strict(snap, project_key, local_folder_name) {
            SnapshotPathMatch::Matched(matched) => return Ok(Some((snap, matched))),
            SnapshotPathMatch::NotFound => continue,
            SnapshotPathMatch::Ambiguous(candidates) => {
                return Err(AppError::InvalidArgument(format!(
                    "最新の旧形式スナップショット（{}）にプロジェクト \"{project_key}\" の候補パスが複数含まれており一意に特定できないため引き継ぎを中止しました: {}",
                    snap.display_short_id(),
                    candidates.join(", ")
                )));
            }
        }
    }
    Ok(None)
}

/// Strict path resolution distinguishing no-match from ambiguous multi-match.
///
/// Unconditionally accepts a single path ONLY when the snapshot carries the matching
/// `project:<project_key>` tag. For untagged legacy snapshots (regardless of `paths.len()`),
/// requires a unique folder-name match.
pub fn resolve_snapshot_source_path_strict<'a>(
    snap: &'a ResticSnapshot,
    project_key: &str,
    local_folder_name: Option<&str>,
) -> SnapshotPathMatch<'a> {
    if snap.project_key() == Some(project_key) && snap.paths.len() == 1 {
        return SnapshotPathMatch::Matched(snap.paths[0].as_str());
    }
    let mut matched: Vec<&'a str> = Vec::new();
    for path in &snap.paths {
        let Some(folder) = snapshot_path_folder_name(path) else {
            continue;
        };
        let by_local_name = local_folder_name
            .is_some_and(|expected_name| folder.eq_ignore_ascii_case(expected_name));
        let by_key = suggest_project_key(folder) == project_key;
        if by_local_name || by_key {
            matched.push(path.as_str());
        }
    }
    match matched.len() {
        0 => SnapshotPathMatch::NotFound,
        1 => SnapshotPathMatch::Matched(matched[0]),
        _ => SnapshotPathMatch::Ambiguous(matched),
    }
}

/// Resolves which path inside `snap.paths` belongs to `project_key`.
pub fn resolve_snapshot_source_path<'a>(
    snap: &'a ResticSnapshot,
    project_key: &str,
    local_folder_name: Option<&str>,
) -> Option<&'a str> {
    match resolve_snapshot_source_path_strict(snap, project_key, local_folder_name) {
        SnapshotPathMatch::Matched(p) => Some(p),
        SnapshotPathMatch::NotFound | SnapshotPathMatch::Ambiguous(_) => None,
    }
}

/// Returns true when another host created a newer effective snapshot than `last_synced_snapshot_id`.
pub fn is_remote_newer(local_machine: &str, row: &ProjectSyncRow) -> bool {
    let Some(remote_id) = row.last_remote_seen_snapshot_id.as_deref() else {
        return false;
    };
    let Some(remote_host) = row.last_remote_seen_host.as_deref() else {
        return false;
    };
    !remote_host.eq_ignore_ascii_case(local_machine)
        && row.last_synced_snapshot_id.as_deref() != Some(remote_id)
}

/// Evaluates remote-newer directly against an in-memory snapshot returned from Restic (avoiding
/// any race with concurrent DB cache updates).
pub fn is_remote_snapshot_newer_than_local(
    local_machine: &str,
    latest_remote: Option<&ResticSnapshot>,
    last_synced_snapshot_id: Option<&str>,
) -> bool {
    let Some(snap) = latest_remote else {
        return false;
    };
    let host = snap.normalized_host();
    !host.is_empty()
        && !host.eq_ignore_ascii_case(local_machine)
        && last_synced_snapshot_id != Some(snap.id.as_str())
}

/// Classifies a configured local target into the multi-PC sync model.
/// Shared targets whose remote state has never been checked return `Unchecked`, and locally
/// released (`standby`) targets return `Standby` instead of falsely inflating `LocalLatest`.
pub fn classify_local_target(
    local_machine: &str,
    target: &BackupTarget,
    row: Option<&ProjectSyncRow>,
) -> ProjectSyncStateKind {
    if target.project_key.is_none() {
        return ProjectSyncStateKind::LocalOnly;
    }
    if row.is_some_and(|r| r.handoff_status != "idle") {
        return ProjectSyncStateKind::Diverged;
    }
    let Some(r) = row.filter(|r| r.last_remote_checked_at.is_some()) else {
        return ProjectSyncStateKind::Unchecked;
    };
    if is_remote_newer(local_machine, r) {
        return if r.local_has_unbacked_changes {
            ProjectSyncStateKind::Diverged
        } else {
            ProjectSyncStateKind::RemoteNewer
        };
    }
    if target.writer_state == WriterState::Standby {
        return ProjectSyncStateKind::Standby;
    }
    ProjectSyncStateKind::LocalLatest
}

/// Suggests `<defaultProjectRoot>\<folderName>` for taking over an unplaced shared project.
pub fn suggest_takeover_destination(
    cfg: &AppConfig,
    source_path: &str,
    project_key: &str,
) -> Option<PathBuf> {
    let root = cfg
        .default_project_root
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())?;
    let folder = snapshot_path_folder_name(source_path).unwrap_or(project_key);
    Some(Path::new(root).join(folder))
}

/// Updates `project_sync_state` (remote observation columns only) and `sync_meta` from a fresh
/// `restic snapshots --json` list without overwriting local sync or active handoff columns.
/// Also stamps `checked_at` for every configured local shared `projectKey` that currently has
/// zero remote snapshots so brand-new shared projects transition out of `Unchecked`.
/// Returns the deduplicated list of remote hosts observed in the repository.
pub fn ingest_snapshots_into_db(
    db: &HistoryDb,
    local_machine: &str,
    snapshots: &[ResticSnapshot],
    checked_at: &str,
    configured_project_keys: &[String],
) -> AppResult<Vec<String>> {
    let mut remote_hosts: BTreeSet<String> = db.observed_remote_hosts()?.into_iter().collect();
    let mut by_key: BTreeMap<String, Vec<&ResticSnapshot>> = BTreeMap::new();

    for snap in snapshots {
        let host = snap.normalized_host();
        if !host.is_empty() && !host.eq_ignore_ascii_case(local_machine) {
            remote_hosts.insert(host);
        }
        if let Some(key) = snap.project_key() {
            by_key.entry(key.to_owned()).or_default().push(snap);
        }
    }

    let mut keys_without_remote: BTreeSet<String> = db
        .list_project_sync_states()?
        .into_iter()
        .map(|r| r.project_key)
        .collect();
    for key in configured_project_keys {
        if !key.trim().is_empty() {
            keys_without_remote.insert(key.clone());
        }
    }

    for (key, group) in &by_key {
        let safety_count = group.iter().filter(|s| s.is_safety_backup()).count() as u32;
        let latest = group
            .iter()
            .copied()
            .filter(|s| !s.is_safety_backup() && !s.is_handoff_staging())
            .max_by(|a, b| compare_snapshot_time(a, b));

        if let Some(snap) = latest {
            let host = snap.normalized_host();
            db.update_remote_snapshot_observation(
                key,
                Some(&snap.id),
                Some(&host),
                Some(&snap.time),
                snap.is_handoff_released(),
                &snap.paths,
                checked_at,
                snap.exclude_fingerprint(),
                safety_count,
            )?;
        } else {
            db.update_remote_snapshot_observation(
                key,
                None,
                None,
                None,
                false,
                &[],
                checked_at,
                None,
                safety_count,
            )?;
        }
    }

    // Clear stale remote cache (and stamp `checked_at` for brand-new configured shared targets
    // with 0 remote snapshots so they no longer remain `Unchecked` after a full refresh).
    for key in keys_without_remote {
        if !by_key.contains_key(&key) {
            db.update_remote_snapshot_observation(
                &key,
                None,
                None,
                None,
                false,
                &[],
                checked_at,
                None,
                0,
            )?;
        }
    }

    let hosts_vec: Vec<String> = remote_hosts.into_iter().collect();
    db.set_observed_remote_hosts(&hosts_vec)?;
    db.set_last_remote_checked_at(checked_at)?;
    Ok(hosts_vec)
}

/// Column-scoped update of remote observation for a single `project_key` from targeted snapshots.
pub fn ingest_single_project_snapshots_into_db(
    db: &HistoryDb,
    local_machine: &str,
    project_key: &str,
    snapshots: &[ResticSnapshot],
    checked_at: &str,
) -> AppResult<()> {
    let mut remote_hosts: BTreeSet<String> = db.observed_remote_hosts()?.into_iter().collect();
    let mut group: Vec<&ResticSnapshot> = Vec::new();
    for snap in snapshots {
        let host = snap.normalized_host();
        if !host.is_empty() && !host.eq_ignore_ascii_case(local_machine) {
            remote_hosts.insert(host);
        }
        if snap.project_key() == Some(project_key) {
            group.push(snap);
        }
    }
    let safety_count = group.iter().filter(|s| s.is_safety_backup()).count() as u32;
    let latest = group
        .iter()
        .copied()
        .filter(|s| !s.is_safety_backup() && !s.is_handoff_staging())
        .max_by(|a, b| compare_snapshot_time(a, b));
    if let Some(snap) = latest {
        let host = snap.normalized_host();
        db.update_remote_snapshot_observation(
            project_key,
            Some(&snap.id),
            Some(&host),
            Some(&snap.time),
            snap.is_handoff_released(),
            &snap.paths,
            checked_at,
            snap.exclude_fingerprint(),
            safety_count,
        )?;
    } else {
        db.update_remote_snapshot_observation(
            project_key,
            None,
            None,
            None,
            false,
            &[],
            checked_at,
            None,
            safety_count,
        )?;
    }
    let hosts_vec: Vec<String> = remote_hosts.into_iter().collect();
    db.set_observed_remote_hosts(&hosts_vec)?;
    Ok(())
}

/// Builds a complete `ProjectSyncReport` purely from local `config.json` and SQLite cache.
pub fn build_sync_report(cfg: &AppConfig, db: &HistoryDb) -> AppResult<ProjectSyncReport> {
    let machine_name = cfg.effective_machine_name();
    let rows = db.list_project_sync_states()?;
    let row_map: BTreeMap<String, ProjectSyncRow> = rows
        .into_iter()
        .map(|r| (r.project_key.clone(), r))
        .collect();
    let last_checked_at = db.last_remote_checked_at()?;
    let observed_remote_hosts = db.observed_remote_hosts()?;

    let mut projects = Vec::new();
    let mut configured_keys = BTreeSet::new();
    let mut summary = ProjectSyncSummary {
        last_checked_at: last_checked_at.clone(),
        ..Default::default()
    };

    for target in &cfg.targets {
        let local_path_ref = Path::new(&target.path);
        let is_git = is_git_repository(local_path_ref);
        let unity_locked = find_unity_lockfile(local_path_ref).is_some();

        let row = target.project_key.as_deref().and_then(|k| row_map.get(k));
        if let Some(k) = &target.project_key {
            configured_keys.insert(k.clone());
        }

        let state = classify_local_target(&machine_name, target, row);
        match state {
            ProjectSyncStateKind::LocalLatest => summary.local_latest += 1,
            ProjectSyncStateKind::RemoteNewer => summary.remote_newer += 1,
            ProjectSyncStateKind::Diverged => summary.diverged += 1,
            ProjectSyncStateKind::LocalOnly => summary.local_only += 1,
            ProjectSyncStateKind::NotLocal => summary.not_local += 1,
            ProjectSyncStateKind::Standby => summary.standby += 1,
            ProjectSyncStateKind::Unchecked => summary.unchecked += 1,
        }

        let local_fp = exclude_fingerprint(cfg, std::slice::from_ref(target));
        let remote_fp = row.and_then(|r| r.last_remote_exclude_fingerprint.as_deref());
        let fp_mismatch = matches!(remote_fp, Some(rfp) if rfp != local_fp);

        let latest_id = row.and_then(|r| r.last_remote_seen_snapshot_id.clone());
        let latest_short = latest_id
            .as_deref()
            .map(|id| id[..id.len().min(8)].to_owned());

        projects.push(ProjectSyncItem {
            project_key: target.project_key.clone(),
            state,
            local_target_id: Some(target.id.clone()),
            label: target.label.clone(),
            local_path: Some(target.path.clone()),
            enabled: target.enabled,
            writer_state: target.project_key.as_ref().map(|_| target.writer_state),
            handoff_status: row
                .map(|r| r.handoff_status.clone())
                .unwrap_or_else(|| "idle".to_owned()),
            latest_snapshot_id: latest_id,
            latest_short_id: latest_short,
            latest_host: row.and_then(|r| r.last_remote_seen_host.clone()),
            latest_time: row.and_then(|r| r.last_remote_seen_at.clone()),
            handoff_ready: row.is_some_and(|r| r.last_remote_seen_released),
            safety_backup_count: row.map_or(0, |r| r.safety_backup_count),
            has_unbacked_local_changes: row.is_some_and(|r| r.local_has_unbacked_changes),
            exclude_fingerprint_mismatch: fp_mismatch,
            remote_paths: row
                .map(|r| r.last_remote_seen_paths.clone())
                .unwrap_or_default(),
            is_git_repository: is_git,
            unity_locked,
            remote_checked: row.is_some_and(|r| r.last_remote_checked_at.is_some()),
            suggested_local_path: None,
        });
    }

    // Add remote-only shared projects (`not_local`).
    for (key, row) in &row_map {
        if configured_keys.contains(key) || row.last_remote_seen_snapshot_id.is_none() {
            continue;
        }
        summary.not_local += 1;
        let first_remote = row
            .last_remote_seen_paths
            .first()
            .map(String::as_str)
            .unwrap_or(key.as_str());
        let label = snapshot_path_folder_name(first_remote)
            .unwrap_or(key.as_str())
            .to_owned();
        let suggested = suggest_takeover_destination(cfg, first_remote, key)
            .map(|p| p.to_string_lossy().into_owned());
        let latest_short = row
            .last_remote_seen_snapshot_id
            .as_deref()
            .map(|id| id[..id.len().min(8)].to_owned());

        projects.push(ProjectSyncItem {
            project_key: Some(key.clone()),
            state: ProjectSyncStateKind::NotLocal,
            local_target_id: None,
            label,
            local_path: None,
            enabled: false,
            writer_state: None,
            handoff_status: row.handoff_status.clone(),
            latest_snapshot_id: row.last_remote_seen_snapshot_id.clone(),
            latest_short_id: latest_short,
            latest_host: row.last_remote_seen_host.clone(),
            latest_time: row.last_remote_seen_at.clone(),
            handoff_ready: row.last_remote_seen_released,
            safety_backup_count: row.safety_backup_count,
            has_unbacked_local_changes: false,
            exclude_fingerprint_mismatch: false,
            remote_paths: row.last_remote_seen_paths.clone(),
            is_git_repository: false,
            unity_locked: false,
            remote_checked: true,
            suggested_local_path: suggested,
        });
    }

    Ok(ProjectSyncReport {
        machine_name,
        maintenance_role: cfg.maintenance_role,
        last_checked_at,
        observed_remote_hosts,
        summary,
        projects,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap(id: &str, time: &str, host: &str, paths: &[&str], tags: &[&str]) -> ResticSnapshot {
        ResticSnapshot {
            id: id.to_owned(),
            short_id: id[..id.len().min(8)].to_owned(),
            original: None,
            time: time.to_owned(),
            hostname: host.to_owned(),
            username: "user".to_owned(),
            paths: paths.iter().map(|s| (*s).to_owned()).collect(),
            tags: tags.iter().map(|s| (*s).to_owned()).collect(),
        }
    }

    #[test]
    fn select_latest_effective_snapshot_ignores_safety_backups() {
        let snaps = vec![
            snap(
                "1111aaaa",
                "2026-11-26T05:00:00Z",
                "pc-a",
                &[r"D:\Projects\ProjectAlpha"],
                &["project:project-alpha", "handoff:released"],
            ),
            snap(
                "2222bbbb",
                "2026-11-26T06:00:00Z",
                "pc-b",
                &[r"E:\Projects\ProjectAlpha"],
                &["project:project-alpha", "handoff:safety-backup"],
            ),
        ];
        let latest = select_latest_effective_snapshot(&snaps, "project-alpha").unwrap();
        assert_eq!(latest.id, "1111aaaa");
        assert!(latest.is_handoff_released());
    }

    #[test]
    fn select_legacy_composite_candidate_matches_folder_name() {
        let snaps = vec![snap(
            "legacy01",
            "2026-11-25T17:00:00Z",
            "pc-a",
            &[r"D:\Projects\ProjectAlpha", r"E:\Media"],
            &[],
        )];
        let (found, path) =
            select_legacy_composite_candidate(&snaps, "project-alpha", Some("ProjectAlpha"))
                .unwrap()
                .unwrap();
        assert_eq!(found.id, "legacy01");
        assert_eq!(path, r"D:\Projects\ProjectAlpha");
    }

    #[test]
    fn ingest_and_build_sync_report_classifies_all_five_states() {
        let dir = tempfile::tempdir().unwrap();
        let db = HistoryDb::open(&dir.path().join("history.db")).unwrap();

        let p_local = dir.path().join("LocalLatest");
        let p_remote = dir.path().join("RemoteNewer");
        let p_diverged = dir.path().join("Diverged");
        let p_only = dir.path().join("LocalOnly");
        for p in [&p_local, &p_remote, &p_diverged, &p_only] {
            std::fs::create_dir_all(p).unwrap();
        }

        let mut cfg = AppConfig {
            machine_name: Some("pc-b".to_owned()),
            default_project_root: Some(r"E:\Projects".to_owned()),
            ..Default::default()
        };
        cfg.upsert_target_full(
            &p_local,
            Some("LocalLatest".to_owned()),
            None,
            None,
            Some(true),
            Some(Some("proj-local".to_owned())),
            Some(WriterState::Active),
        )
        .unwrap();
        cfg.upsert_target_full(
            &p_remote,
            Some("RemoteNewer".to_owned()),
            None,
            None,
            Some(true),
            Some(Some("proj-remote".to_owned())),
            Some(WriterState::Standby),
        )
        .unwrap();
        cfg.upsert_target_full(
            &p_diverged,
            Some("Diverged".to_owned()),
            None,
            None,
            Some(true),
            Some(Some("proj-diverged".to_owned())),
            Some(WriterState::Active),
        )
        .unwrap();
        cfg.upsert_target(
            &p_only,
            Some("LocalOnly".to_owned()),
            None,
            None,
            Some(true),
        )
        .unwrap();

        // Mark proj-diverged as having unbacked local changes.
        let mut div_row = ProjectSyncRow::new("proj-diverged");
        div_row.local_has_unbacked_changes = true;
        db.upsert_project_sync_state(&div_row).unwrap();

        let snaps = vec![
            snap(
                "snap-local-1",
                "2026-11-26T05:00:00Z",
                "pc-b",
                &[r"E:\Projects\LocalLatest"],
                &["project:proj-local"],
            ),
            snap(
                "snap-remote-1",
                "2026-11-26T05:00:00Z",
                "pc-a",
                &[r"D:\Projects\RemoteNewer"],
                &["project:proj-remote", "handoff:released"],
            ),
            snap(
                "snap-div-1",
                "2026-11-26T05:00:00Z",
                "pc-a",
                &[r"D:\Projects\Diverged"],
                &["project:proj-diverged"],
            ),
            snap(
                "snap-notlocal-1",
                "2026-11-26T05:00:00Z",
                "pc-a",
                &[r"D:\Projects\ProjectBeta"],
                &["project:project-beta", "handoff:released"],
            ),
        ];

        let configured_keys: Vec<String> = cfg
            .targets
            .iter()
            .filter_map(|t| t.project_key.clone())
            .collect();
        let hosts = ingest_snapshots_into_db(
            &db,
            "pc-b",
            &snaps,
            "2026-11-26T06:00:00Z",
            &configured_keys,
        )
        .unwrap();
        assert_eq!(hosts, vec!["pc-a".to_owned()]);

        let report = build_sync_report(&cfg, &db).unwrap();
        assert_eq!(report.summary.local_latest, 1);
        assert_eq!(report.summary.remote_newer, 1);
        assert_eq!(report.summary.diverged, 1);
        assert_eq!(report.summary.not_local, 1);
        assert_eq!(report.summary.local_only, 1);

        let not_local = report
            .projects
            .iter()
            .find(|p| p.project_key.as_deref() == Some("project-beta"))
            .unwrap();
        assert_eq!(not_local.state, ProjectSyncStateKind::NotLocal);
        assert_eq!(
            not_local.suggested_local_path.as_deref(),
            Some(r"E:\Projects\ProjectBeta")
        );

        // ingest_snapshots_into_db must not clobber active handoff checkpoint columns.
        db.set_handoff_state(
            "proj-remote",
            "restoring",
            Some("snap-remote-1"),
            Some(r"E:\Projects\RemoteNewer"),
            Some("safety-1"),
        )
        .unwrap();
        ingest_snapshots_into_db(
            &db,
            "pc-b",
            &snaps,
            "2026-11-26T07:00:00Z",
            &configured_keys,
        )
        .unwrap();
        let after_ingest = db.get_project_sync_state("proj-remote").unwrap().unwrap();
        assert_eq!(after_ingest.handoff_status, "restoring");
        assert_eq!(
            after_ingest.handoff_source_snapshot_id.as_deref(),
            Some("snap-remote-1")
        );
        assert_eq!(
            after_ingest.handoff_target_path.as_deref(),
            Some(r"E:\Projects\RemoteNewer")
        );
        assert_eq!(
            after_ingest.handoff_safety_snapshot_id.as_deref(),
            Some("safety-1")
        );

        // Untagged single-path local-only snapshot must not hijack legacy takeover.
        let unrelated_local = snap(
            "snap-unrelated",
            "2026-11-26T09:00:00Z",
            "pc-a",
            &[r"D:\Notes\Personal"],
            &[],
        );
        let matching_legacy = snap(
            "snap-legacy-match",
            "2026-11-25T05:00:00Z",
            "pc-a",
            &[r"D:\Projects\ProjectAlpha", r"D:\Projects\Other"],
            &[],
        );
        let snaps = [unrelated_local, matching_legacy.clone()];
        let (picked, path) =
            select_legacy_composite_candidate(&snaps, "project-alpha", Some("ProjectAlpha"))
                .unwrap()
                .expect("must skip unrelated 1-path snap and select matching legacy composite");
        assert_eq!(picked.id, "snap-legacy-match");
        assert_eq!(path, r"D:\Projects\ProjectAlpha");

        // Ambiguous newest legacy snapshot must return Err rather than falling through to older ones.
        let ambiguous = snap(
            "snap-ambig",
            "2026-11-26T10:00:00Z",
            "pc-a",
            &[r"D:\A\ProjectAlpha", r"D:\B\ProjectAlpha"],
            &[],
        );
        assert!(
            resolve_snapshot_source_path(&ambiguous, "project-alpha", Some("ProjectAlpha"))
                .is_none()
        );
        let ambig_and_older = [ambiguous, matching_legacy];
        assert!(
            select_legacy_composite_candidate(
                &ambig_and_older,
                "project-alpha",
                Some("ProjectAlpha")
            )
            .is_err(),
            "ambiguous newest legacy snapshot must abort instead of falling back to an older snapshot"
        );

        // Unchecked shared target and locally-released standby target must not be classified as LocalLatest.
        let p_unchecked = dir.path().join("Unchecked");
        let p_standby = dir.path().join("ReleasedStandby");
        std::fs::create_dir_all(&p_unchecked).unwrap();
        std::fs::create_dir_all(&p_standby).unwrap();
        cfg.upsert_target_full(
            &p_unchecked,
            Some("Unchecked".to_owned()),
            None,
            None,
            Some(true),
            Some(Some("proj-unchecked".to_owned())),
            Some(WriterState::Active),
        )
        .unwrap();
        cfg.upsert_target_full(
            &p_standby,
            Some("ReleasedStandby".to_owned()),
            None,
            None,
            Some(true),
            Some(Some("proj-standby".to_owned())),
            Some(WriterState::Standby),
        )
        .unwrap();
        db.update_local_backup_sync(
            "proj-standby",
            "snap-standby-1",
            "2026-11-26T08:00:00Z",
            "pc-b",
            true,
            &p_standby.to_string_lossy(),
            None,
        )
        .unwrap();
        let report2 = build_sync_report(&cfg, &db).unwrap();
        assert_eq!(report2.summary.local_latest, 1);
        assert_eq!(report2.summary.unchecked, 1);
        assert_eq!(report2.summary.standby, 1);

        // After full refresh with configured keys, brand-new shared project
        // with 0 remote snapshots transitions from `Unchecked` -> `LocalLatest` (`remote_checked = true`).
        let all_keys: Vec<String> = cfg
            .targets
            .iter()
            .filter_map(|t| t.project_key.clone())
            .collect();
        ingest_snapshots_into_db(&db, "pc-b", &snaps, "2026-11-26T09:00:00Z", &all_keys).unwrap();
        let report3 = build_sync_report(&cfg, &db).unwrap();
        assert_eq!(report3.summary.unchecked, 0);
        let checked_zero = report3
            .projects
            .iter()
            .find(|p| p.project_key.as_deref() == Some("proj-unchecked"))
            .unwrap();
        assert!(checked_zero.remote_checked);
        assert_eq!(checked_zero.state, ProjectSyncStateKind::LocalLatest);
    }
}
