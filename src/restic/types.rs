//! Typed representations of Restic `--json` output structures.

use serde::{Deserialize, Serialize};

/// A snapshot item returned by `restic snapshots --json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResticSnapshot {
    pub id: String,
    #[serde(default)]
    pub short_id: String,
    #[serde(default)]
    pub original: Option<String>,
    pub time: String,
    #[serde(default)]
    pub hostname: String,
    #[serde(default)]
    pub username: String,
    #[serde(default)]
    pub paths: Vec<String>,
    #[serde(default)]
    pub tags: Vec<String>,
}

pub const TAG_PROJECT_PREFIX: &str = "project:";
pub const TAG_HANDOFF_RELEASED: &str = "handoff:released";
pub const TAG_HANDOFF_CLAIMED: &str = "handoff:claimed";
pub const TAG_HANDOFF_SAFETY_BACKUP: &str = "handoff:safety-backup";
pub const TAG_HANDOFF_STAGING: &str = "handoff:staging";
pub const TAG_HANDOFF_FROM_PREFIX: &str = "handoff:from:";
pub const TAG_EXCLUDES_PREFIX: &str = "excludes:";

impl ResticSnapshot {
    /// Returns the `projectKey` if this snapshot carries a `project:<key>` tag.
    pub fn project_key(&self) -> Option<&str> {
        self.tags
            .iter()
            .find_map(|t| t.strip_prefix(TAG_PROJECT_PREFIX))
            .filter(|k| !k.is_empty())
    }

    /// True when this snapshot is a pre-takeover safety backup (`handoff:safety-backup`).
    pub fn is_safety_backup(&self) -> bool {
        self.tags.iter().any(|t| t == TAG_HANDOFF_SAFETY_BACKUP)
    }

    /// True when this snapshot is an unpublished staging snapshot (`handoff:staging`).
    pub fn is_handoff_staging(&self) -> bool {
        self.tags.iter().any(|t| t == TAG_HANDOFF_STAGING)
    }

    /// True when the previous owner explicitly released the project (`handoff:released`).
    pub fn is_handoff_released(&self) -> bool {
        self.tags.iter().any(|t| t == TAG_HANDOFF_RELEASED)
    }

    /// Returns the source snapshot ID this claim was taken over from (`handoff:from:<id>`), if any.
    pub fn handoff_from_snapshot_id(&self) -> Option<&str> {
        self.tags
            .iter()
            .find_map(|t| t.strip_prefix(TAG_HANDOFF_FROM_PREFIX))
            .filter(|id| !id.is_empty())
    }

    /// Returns the exclude settings fingerprint if recorded via `excludes:<hex>` tag.
    pub fn exclude_fingerprint(&self) -> Option<&str> {
        self.tags
            .iter()
            .find_map(|t| t.strip_prefix(TAG_EXCLUDES_PREFIX))
            .filter(|fp| !fp.is_empty())
    }

    /// Lowercase normalized hostname for case-insensitive Windows host comparison.
    pub fn normalized_host(&self) -> String {
        self.hostname.trim().to_ascii_lowercase()
    }

    pub fn display_short_id(&self) -> &str {
        if self.short_id.is_empty() {
            &self.id[..self.id.len().min(8)]
        } else {
            &self.short_id
        }
    }
}

/// Real-time progress line emitted during `restic backup --json` or handoff `restore`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BackupProgressState {
    pub running: bool,
    pub target_label: String,
    pub percent_done: f64,
    pub files_done: u64,
    pub total_files: u64,
    pub bytes_done: u64,
    pub total_bytes: u64,
    pub current_file: String,
    /// True while Restic is waiting for another PC's exclusive repository lock via `--retry-lock`.
    #[serde(default)]
    pub waiting_for_lock: bool,
    /// Optional status message shown in the progress UI (e.g. lock wait or handoff stage).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status_note: Option<String>,
}

/// Summary object emitted at the end of `restic backup --json`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ResticBackupSummary {
    #[serde(default)]
    pub files_new: u64,
    #[serde(default)]
    pub files_changed: u64,
    #[serde(default)]
    pub files_unmodified: u64,
    #[serde(default)]
    pub dirs_new: u64,
    #[serde(default)]
    pub dirs_changed: u64,
    #[serde(default)]
    pub dirs_unmodified: u64,
    #[serde(default)]
    pub data_added: u64,
    #[serde(default)]
    pub total_files_processed: u64,
    #[serde(default)]
    pub total_bytes_processed: u64,
    #[serde(default)]
    pub total_duration: f64,
    #[serde(default)]
    pub snapshot_id: Option<String>,
    /// Source paths restic could not read (exit code 3, snapshot still saved). Set by the
    /// app from restic's error lines, not part of restic's summary.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unreadable: Vec<String>,
    /// True when `restic backup` exited with code 3 (partial read warnings).
    #[serde(skip)]
    pub partial_read_warning: bool,
    /// Folders whose new or modified files stored the most data in the repository this run,
    /// largest first. Set by the app from restic's `-vv` lines, not part of restic's summary.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub top_added: Vec<FolderBytes>,
    /// Hash of the exclude settings this run used, to tell when they changed between runs.
    /// Set by the app; kept out of tool output.
    #[serde(default, skip_serializing)]
    pub exclude_fingerprint: Option<String>,
}

/// Bytes attributed to one folder, two levels below its backup target at most: what a run
/// stored for it (`top_added`), or the size of its files in a snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FolderBytes {
    pub path: String,
    pub bytes: u64,
}

/// A file/directory entry emitted by `restic ls <snapshot> --json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResticLsEntry {
    pub path: String,
    #[serde(rename = "type", default)]
    pub kind: String,
    #[serde(default)]
    pub size: Option<u64>,
    #[serde(default)]
    pub mtime: Option<String>,
}

/// Statistics object emitted by `restic diff <s1> <s2> --json`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResticDiffReport {
    pub source_snapshot: String,
    pub target_snapshot: String,
    pub added_files: u64,
    pub removed_files: u64,
    pub modified_files: u64,
    pub added_bytes: u64,
    pub removed_bytes: u64,
    pub changed_paths: Vec<String>,
}
