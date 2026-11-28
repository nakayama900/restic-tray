//! Declarative JSON configuration (`config.json`) for Restic Tray.
//!
//! Can be edited directly as JSON, through the GUI, or via the built-in MCP server.
//! Secrets are never stored in this file; they live in `credentials.dat` (`crate::secrets`).

use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use chrono::NaiveTime;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::error::{AppError, AppResult};
use crate::fs_safety::{canonicalize_existing_dir, is_git_repository};

/// Application identifier (`AppUserModelID` and `%LOCALAPPDATA%` directory name).
/// Managed as the single source of truth for runtime data storage and Windows toast notifications.
pub const APP_IDENTIFIER: &str = "io.github.nayuengin.restic-tray";
pub const CONFIG_FILENAME: &str = "config.json";
pub const CONFIG_PKL_FILENAME: &str = "config.pkl";
pub const MIN_INTERVAL_MINUTES: u64 = 1;
pub const MAX_INTERVAL_MINUTES: u64 = 10_080;
pub const MAX_KEEP_LAST: u32 = 1_000;
pub const MAX_KEEP_DAILY: u32 = 365;
pub const MAX_KEEP_WEEKLY: u32 = 520;
pub const MAX_KEEP_MONTHLY: u32 = 240;

/// Consumes any deprecated JSON value on load and drops it; never serialized.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DeprecatedConfigField;

impl<'de> Deserialize<'de> for DeprecatedConfigField {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let _ = serde::de::IgnoredAny::deserialize(deserializer)?;
        Ok(Self)
    }
}

/// Dedicated configuration error shown when `config.json` selects the removed Google Drive backend.
pub fn google_drive_removed_config_error(path: &Path) -> String {
    format!(
        "Google Drive バックエンドは廃止されており、現在の Restic Tray は Cloudflare R2 のみに対応しています。\n\
         意図しない保存先への自動バックアップを防ぐため、Cloudflare R2 への自動切り替えは行わずに起動を停止しています。\n\n\
         設定ファイル: {}\n\n\
         対処方法:\n\
         ・Cloudflare R2 へ切り替える場合: 上記の config.json から \"storageBackend\": \"googleDrive\" の行を削除（または \"r2\" に変更）し、R2 の設定を確認してから再起動してください。\n\
         ・既存の Google Drive 上のバックアップは標準の restic リポジトリのため、restic と rclone を直接使ってデータを取り出せます。",
        path.display()
    )
}

/// Whether this PC runs automatic repository maintenance (`forget --prune` and periodic `check`).
///
/// Only one PC sharing a repository should use `Primary`; secondary PCs should use `None`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum MaintenanceRole {
    #[default]
    Primary,
    None,
}

impl MaintenanceRole {
    pub fn is_default(&self) -> bool {
        *self == Self::Primary
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Primary => "primary",
            Self::None => "none",
        }
    }
}

/// Single-writer state for a shared project (`projectKey` set) on this PC.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum WriterState {
    #[default]
    Active,
    Standby,
}

impl WriterState {
    pub fn is_default(&self) -> bool {
        *self == Self::Active
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Standby => "standby",
        }
    }
}

/// Storage backend that owns the Restic repository. `GoogleDrive` is the removed v0.4–v0.7
/// backend selector kept only to produce the dedicated config error on load.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum StorageBackend {
    #[default]
    R2,
    Rclone,
    #[serde(rename = "googleDrive")]
    GoogleDrive,
}

impl StorageBackend {
    pub fn is_default(&self) -> bool {
        *self == Self::R2
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::R2 => "r2",
            Self::Rclone => "rclone",
            Self::GoogleDrive => "googleDrive",
        }
    }
}

/// Rclone adapter settings (non-secret). Restic Tray runs `rclone serve s3` as a loopback-only
/// local S3 adapter (`--addr 127.0.0.1:<port>`, anonymous access) in front of any rclone remote,
/// so Restic keeps using its standard S3 backend. Pointing `remote` at a `crypt:` remote gives
/// client-side encryption via Rclone Crypto on top of Restic's own encryption. Rclone keeps its
/// own credentials in `rclone.conf`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RcloneConfig {
    /// Served rclone remote, e.g. `mycrypt:` or `gdrive:backups`.
    #[serde(default)]
    pub remote: String,
    /// S3 bucket name Restic uses; `serve s3` maps buckets to directories under the remote root.
    #[serde(default = "default_rclone_bucket")]
    pub bucket: String,
    /// Fixed loopback port for the adapter; picked at runtime when unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    /// Optional `rclone.exe` override; its folder is prepended to `PATH` for the adapter.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub binary_path: Option<String>,
    /// Optional `rclone.conf` override passed via the `RCLONE_CONFIG` environment variable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_path: Option<String>,
}

fn default_rclone_bucket() -> String {
    "restic".to_owned()
}

impl RcloneConfig {
    /// Normalizes the served remote: trimmed, forward slashes, no trailing slash on the path part.
    /// The path may be empty (`mycrypt:` serves the remote root).
    pub fn normalized_remote(&self) -> String {
        let trimmed = self.remote.trim().replace('\\', "/");
        match trimmed.split_once(':') {
            Some((name, path)) => {
                let path = path.trim_end_matches('/');
                if path.is_empty() {
                    format!("{name}:")
                } else {
                    format!("{name}:{path}")
                }
            }
            None => trimmed,
        }
    }

    /// Validates the `<remote>[:<path>]` form: an rclone remote name before the first colon and a
    /// path without whitespace, control characters, or `..` segments after it.
    pub fn validate_remote(&self) -> AppResult<String> {
        let normalized = self.normalized_remote();
        let Some((name, path)) = normalized.split_once(':') else {
            return Err(AppError::Config(
                "rclone.remote must be an rclone remote such as \"mycrypt:\" or \"gdrive:backups\""
                    .to_owned(),
            ));
        };
        let name_chars_ok = !name.is_empty()
            && name.chars().enumerate().all(|(i, c)| {
                c.is_ascii_alphanumeric() || c == '_' || c == '-' || (i > 0 && c == '.')
            });
        if !name_chars_ok {
            return Err(AppError::Config(format!(
                "rclone.remote \"{normalized}\": the remote name before \":\" must be alphanumeric characters, \"_\", \"-\", or \".\""
            )));
        }
        let segments: Vec<&str> = path.split('/').collect();
        let path_segments = path
            .strip_prefix('/')
            .map(|_| &segments[1..])
            .unwrap_or(&segments);
        if path.chars().any(|c| c.is_whitespace() || c.is_control())
            || path_segments
                .iter()
                .any(|seg| seg.is_empty() || *seg == "..")
        {
            return Err(AppError::Config(format!(
                "rclone.remote \"{normalized}\": the path must not contain whitespace or \"..\" segments"
            )));
        }
        Ok(normalized)
    }

    /// The S3 bucket name Restic uses. `serve s3` maps buckets to directories under the remote
    /// root, so this becomes a folder in the served remote.
    pub fn normalized_bucket(&self) -> String {
        self.bucket.trim().trim_matches('/').to_owned()
    }

    pub fn is_configured(&self) -> bool {
        !self.remote.trim().is_empty() && !self.normalized_bucket().is_empty()
    }

    pub fn is_empty(&self) -> bool {
        self.remote.trim().is_empty()
            && self.bucket == default_rclone_bucket()
            && self.port.is_none()
            && self.binary_path.is_none()
            && self.config_path.is_none()
    }

    /// Human-readable storage description for status display. The actual endpoint is decided at
    /// runtime by the `rclone serve s3` adapter (see [`crate::rclone`]).
    pub fn repository_display(&self) -> AppResult<String> {
        let normalized = self.validate_remote()?;
        Ok(format!(
            "rclone serve s3 -> {} (bucket: {})",
            normalized,
            self.normalized_bucket()
        ))
    }
}

/// Root configuration structure persisted in `config.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AppConfig {
    #[serde(default = "default_schema_version")]
    pub schema_version: u32,
    /// Storage backend selector (`"r2"` default, `"rclone"` for the restic rclone backend
    /// incl. `crypt:` remotes via Rclone Crypto). Omitted when `r2`.
    #[serde(
        default,
        rename = "storageBackend",
        skip_serializing_if = "StorageBackend::is_default"
    )]
    pub storage_backend: StorageBackend,
    /// Stable lowercase PC name passed to `restic backup --host`. Defaults to lowercase OS hostname.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub machine_name: Option<String>,
    /// Default parent folder proposed when taking over a shared project not yet placed on this PC.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_project_root: Option<String>,
    /// Whether this PC performs automatic `forget --prune` and periodic `check`.
    #[serde(default, skip_serializing_if = "MaintenanceRole::is_default")]
    pub maintenance_role: MaintenanceRole,
    #[serde(default)]
    pub r2: R2Config,
    /// Rclone backend settings used when `storageBackend` is `"rclone"`.
    #[serde(default, skip_serializing_if = "RcloneConfig::is_empty")]
    pub rclone: RcloneConfig,
    /// Deprecated v0.4–v0.7 Google Drive section accepted on load and dropped; never written back.
    #[serde(default, skip_serializing)]
    pub google_drive: DeprecatedConfigField,
    #[serde(default)]
    pub targets: Vec<BackupTarget>,
    /// Applies `BASIC_EXCLUDES` and `BASIC_EXCLUDE_IF_PRESENT` on top of the lists below.
    #[serde(default = "default_true")]
    pub use_basic_excludes: bool,
    #[serde(default)]
    pub global_excludes: Vec<String>,
    /// `restic --exclude-if-present` markers (`filename[:header]`): skips the contents of any
    /// folder holding that file, e.g. `SourceAssetDB` for Unity's `Library`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exclude_if_present: Vec<String>,
    /// Honors each backup target's root `.gitignore` (gitignore syntax) during backup and
    /// restore. Nested `.gitignore` files are not read.
    #[serde(default)]
    pub use_gitignore: bool,
    /// Additional ignore files in gitignore syntax, resolved relative to each backup target
    /// (or absolute), e.g. `.gitignore`-style `.resticignore`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ignore_files: Vec<String>,
    #[serde(default)]
    pub schedule: ScheduleConfig,
    #[serde(default)]
    pub retention: RetentionPolicy,
    #[serde(default)]
    pub agent: AgentPolicy,
    #[serde(default)]
    pub restic_binary_path: Option<String>,
    /// Deprecated v0.4–v0.7 `rclone.exe` override accepted on load and dropped; never written back.
    #[serde(default, skip_serializing)]
    pub rclone_binary_path: DeprecatedConfigField,
    #[serde(default = "default_theme")]
    pub theme: String,
    #[serde(default)]
    pub start_minimized_to_tray: bool,
}

fn default_schema_version() -> u32 {
    1
}

/// Built-in `restic --exclude` patterns for things tools recreate: dependency and build caches,
/// temporary files, and Unity's editor logs. They match at any depth, so only names that are
/// never real content belong here.
/// Kept out of `config.json` so an app update can extend the list for existing installs.
pub const BASIC_EXCLUDES: &[&str] = &[
    "target/",
    "node_modules/",
    ".venv/",
    ".pytest_cache/",
    "__pycache__/",
    ".git/objects/pack/tmp_pack_*",
    "*.tmp",
    "*.part",
    ".next/",
    "shadercompiler-*.log",
    "AssetImportWorker*.log",
];

/// Folders Unity or its builds recreate, excluded only directly under a Unity project (a folder
/// with `ProjectSettings\ProjectVersion.txt`). The same names deeper down are often real content,
/// such as a package's `Editor\Build` scripts or `SerializedUdonPrograms` shipped in a package.
pub const BASIC_UNITY_PROJECT_EXCLUDES: &[&str] = &[
    "Library",
    "Temp",
    "Logs",
    "obj",
    "Build",
    "Builds",
    r"Assets\SerializedUdonPrograms",
];

/// Built-in `restic --exclude-if-present` markers: Unity's `Library` (`SourceAssetDB`), Unity's
/// `Temp` while the editor is open (`UnityLockfile`), tagged caches and Python virtual envs.
pub const BASIC_EXCLUDE_IF_PRESENT: &[&str] = &[
    "SourceAssetDB",
    "UnityLockfile",
    "CACHEDIR.TAG:Signature: 8a477f597d28d172789f06886806bc55",
    "pyvenv.cfg",
];

fn with_basic<'a>(basic: &[&'static str], own: &'a [String]) -> Vec<&'a str> {
    basic
        .iter()
        .copied()
        .chain(own.iter().map(String::as_str))
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect()
}

fn default_theme() -> String {
    "dark".to_owned()
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            schema_version: default_schema_version(),
            storage_backend: StorageBackend::default(),
            machine_name: None,
            default_project_root: None,
            maintenance_role: MaintenanceRole::default(),
            r2: R2Config::default(),
            rclone: RcloneConfig::default(),
            google_drive: DeprecatedConfigField,
            targets: Vec::new(),
            use_basic_excludes: true,
            global_excludes: Vec::new(),
            exclude_if_present: Vec::new(),
            use_gitignore: false,
            ignore_files: Vec::new(),
            schedule: ScheduleConfig::default(),
            retention: RetentionPolicy::default(),
            agent: AgentPolicy::default(),
            restic_binary_path: None,
            rclone_binary_path: DeprecatedConfigField,
            theme: default_theme(),
            start_minimized_to_tray: false,
        }
    }
}

/// Cloudflare R2 bucket and endpoint settings (non-secret).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct R2Config {
    pub account_id: String,
    pub bucket: String,
    #[serde(default)]
    pub prefix: String,
    #[serde(default)]
    pub endpoint_override: Option<String>,
    #[serde(default = "default_r2_region")]
    pub region: String,
    #[serde(default = "default_r2_connections")]
    pub max_connections: u32,
}

fn default_r2_region() -> String {
    "auto".to_owned()
}

fn default_r2_connections() -> u32 {
    10
}

impl Default for R2Config {
    fn default() -> Self {
        Self {
            account_id: String::new(),
            bucket: String::new(),
            prefix: "restic-windows".to_owned(),
            endpoint_override: None,
            region: default_r2_region(),
            max_connections: default_r2_connections(),
        }
    }
}

impl R2Config {
    pub fn normalized_bucket(&self) -> String {
        self.bucket.trim().trim_matches('/').to_owned()
    }

    pub fn normalized_prefix(&self) -> String {
        self.prefix
            .replace('\\', "/")
            .trim_matches(|c: char| c == '/' || c.is_whitespace())
            .to_owned()
    }

    pub fn is_configured(&self) -> bool {
        (!self.account_id.trim().is_empty()
            || self
                .endpoint_override
                .as_deref()
                .is_some_and(|s| !s.trim().is_empty()))
            && !self.normalized_bucket().is_empty()
    }

    /// Resolves the S3-compatible endpoint URL (`https://<account_id>.r2.cloudflarestorage.com`
    /// unless `endpoint_override` is set).
    pub fn endpoint_url(&self) -> AppResult<String> {
        match self
            .endpoint_override
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            Some(custom) => Ok(custom.trim_end_matches('/').to_owned()),
            None => {
                let acc = self.account_id.trim();
                if acc.is_empty() {
                    return Err(AppError::Config(
                        "Cloudflare R2 accountId and bucket must be configured".to_owned(),
                    ));
                }
                if acc
                    .chars()
                    .any(|c| c.is_whitespace() || matches!(c, '/' | '?' | '#' | '@' | ':'))
                {
                    return Err(AppError::Config(
                        "Cloudflare R2 accountId contains invalid characters".to_owned(),
                    ));
                }
                Ok(format!("https://{acc}.r2.cloudflarestorage.com"))
            }
        }
    }

    /// Formats the Restic S3 repository string for Cloudflare R2:
    /// `s3:https://<account_id>.r2.cloudflarestorage.com/<bucket>[/<prefix>]`
    pub fn repository_url(&self) -> AppResult<String> {
        if !self.is_configured() {
            return Err(AppError::Config(
                "Cloudflare R2 accountId and bucket must be configured".to_owned(),
            ));
        }
        let endpoint = self.endpoint_url()?;
        let bucket = self.normalized_bucket();
        let prefix = self.normalized_prefix();
        if prefix.is_empty() {
            Ok(format!("s3:{endpoint}/{bucket}"))
        } else {
            Ok(format!("s3:{endpoint}/{bucket}/{prefix}"))
        }
    }
}

/// Validates a shared project key (`^[a-z0-9][a-z0-9._-]{0,63}$`).
pub fn validate_project_key(raw: &str) -> AppResult<String> {
    let key = raw.trim();
    if key.is_empty() || key.len() > 64 {
        return Err(AppError::InvalidArgument(
            "projectKey は 1〜64 文字の英小文字・数字・ドット・ハイフン・アンダースコアで指定してください"
                .to_owned(),
        ));
    }
    let bytes = key.as_bytes();
    let valid_first = bytes[0].is_ascii_lowercase() || bytes[0].is_ascii_digit();
    let valid_rest = bytes
        .iter()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'.' | b'_' | b'-'));
    if !valid_first || !valid_rest {
        return Err(AppError::InvalidArgument(format!(
            "projectKey \"{key}\" は不正です（先頭は英小文字または数字、使用可能文字は a-z, 0-9, ., _, -、最大 64 文字）"
        )));
    }
    Ok(key.to_owned())
}

/// Generates a default valid `projectKey` candidate from a folder or target label name.
pub fn suggest_project_key(name: &str) -> String {
    let mut out = String::new();
    let mut prev_sep = false;
    for ch in name.trim().chars() {
        let lower = ch.to_ascii_lowercase();
        if lower.is_ascii_lowercase() || lower.is_ascii_digit() {
            out.push(lower);
            prev_sep = false;
        } else if matches!(lower, '.' | '_' | '-') {
            if !out.is_empty() && !prev_sep {
                out.push(lower);
                prev_sep = true;
            }
        } else if !out.is_empty() && !prev_sep {
            out.push('-');
            prev_sep = true;
        }
    }
    while out.ends_with(['-', '_', '.']) {
        out.pop();
    }
    if out.len() > 64 {
        out.truncate(64);
        while out.ends_with(['-', '_', '.']) {
            out.pop();
        }
    }
    if out.is_empty() || !out.as_bytes()[0].is_ascii_alphanumeric() {
        "project".to_owned()
    } else {
        out
    }
}

/// Reserved tag prefixes managed exclusively by Restic Tray's multi-PC coordination layer.
pub const RESERVED_TAG_PREFIXES: &[&str] = &["project:", "handoff:", "excludes:"];

/// Validates free-form user/MCP tags, rejecting commas (which Restic splits into multiple tags),
/// control characters, and reserved multi-PC namespaces (`project:`, `handoff:`, `excludes:`).
pub fn validate_user_tags(tags: &[String]) -> AppResult<Vec<String>> {
    let mut clean = Vec::new();
    for raw in tags {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            continue;
        }
        if trimmed.chars().any(char::is_control) {
            return Err(AppError::InvalidArgument(
                "タグに制御文字は使用できません".to_owned(),
            ));
        }
        if trimmed.contains(',') {
            return Err(AppError::InvalidArgument(format!(
                "タグ \"{trimmed}\" にカンマは使用できません（複数のタグは配列で個別に指定してください）"
            )));
        }
        let lower = trimmed.to_ascii_lowercase();
        if RESERVED_TAG_PREFIXES
            .iter()
            .any(|prefix| lower.starts_with(prefix))
        {
            return Err(AppError::InvalidArgument(format!(
                "タグ \"{trimmed}\" は内部予約名前空間（project: / handoff: / excludes:）のため自由タグとして指定できません"
            )));
        }
        if !clean.iter().any(|existing| existing == trimmed) {
            clean.push(trimmed.to_owned());
        }
    }
    Ok(clean)
}

/// Normalizes and validates an explicit `machineName` (lowercase, trimmed, no control/path chars).
pub fn normalize_machine_name(raw: &str) -> AppResult<String> {
    let name = raw.trim().to_ascii_lowercase();
    if name.is_empty() || name.len() > 64 {
        return Err(AppError::InvalidArgument(
            "machineName は 1〜64 文字で指定してください".to_owned(),
        ));
    }
    if name
        .chars()
        .any(|c| c.is_control() || c.is_whitespace() || matches!(c, '/' | '\\' | ':'))
    {
        return Err(AppError::InvalidArgument(format!(
            "machineName \"{name}\" に使用できない文字が含まれています"
        )));
    }
    Ok(name)
}

/// Resolves the OS hostname normalized to lowercase for `--host`.
pub fn default_os_machine_name() -> String {
    std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .ok()
        .map(|s| s.trim().to_ascii_lowercase())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "local-pc".to_owned())
}

/// A single directory target to back up with Restic.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BackupTarget {
    pub id: String,
    pub label: String,
    pub path: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Stable logical project identifier shared across PCs (`project:<key>` tag).
    /// `None` marks this folder as local-only (not shown in cross-PC handoff).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_key: Option<String>,
    /// Single-writer state for shared projects: only `Active` targets run in automatic backups.
    #[serde(default, skip_serializing_if = "WriterState::is_default")]
    pub writer_state: WriterState,
    #[serde(default)]
    pub exclude_patterns: Vec<String>,
    #[serde(default)]
    pub tags: Vec<String>,
}

fn default_true() -> bool {
    true
}

impl BackupTarget {
    pub fn new(label: impl Into<String>, canonical_path: &Path) -> Self {
        let path_str = canonical_path.to_string_lossy().into_owned();
        let label_str = label.into();
        let final_label = if label_str.trim().is_empty() {
            canonical_path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| path_str.clone())
        } else {
            label_str
        };
        Self {
            id: Uuid::new_v4().simple().to_string()[..12].to_owned(),
            label: final_label,
            path: path_str,
            enabled: true,
            project_key: None,
            writer_state: WriterState::Active,
            exclude_patterns: Vec::new(),
            tags: Vec::new(),
        }
    }

    /// True when this target participates in automatic scheduled backups on this PC.
    pub fn is_active_for_backup(&self) -> bool {
        self.enabled && (self.project_key.is_none() || self.writer_state == WriterState::Active)
    }
}

/// Periodic backup schedule settings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ScheduleConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_interval_minutes")]
    pub interval_minutes: u64,
    #[serde(default)]
    pub run_on_startup: bool,
    #[serde(default)]
    pub auto_prune_after_backup: bool,
    /// Local times ("HH:MM", 24-hour) to run each day. When non-empty, `interval_minutes` is unused.
    #[serde(
        default,
        deserialize_with = "deserialize_daily_at",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub daily_at: Vec<String>,
    /// Days between restore checks after a scheduled backup; 0 turns them off.
    /// These three are left out of `config.json` at their defaults, so older builds can read it.
    #[serde(
        default = "default_verify_every_days",
        skip_serializing_if = "is_default_verify_every_days"
    )]
    pub verify_every_days: u32,
    /// Share of the stored data each restore check downloads and verifies, in percent.
    #[serde(
        default = "default_verify_read_percent",
        skip_serializing_if = "is_default_verify_read_percent"
    )]
    pub verify_read_percent: u32,
    /// Hours without a successful backup before a Windows notification; 0 turns it off.
    #[serde(
        default = "default_notify_overdue_hours",
        skip_serializing_if = "is_default_notify_overdue_hours"
    )]
    pub notify_overdue_hours: u32,
    /// Optional daily local time ("HH:MM", 24-hour) for running exclusive maintenance
    /// (`forget --prune` and due `verify`) on `maintenanceRole: "primary"` PCs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub maintenance_at: Option<String>,
}

pub fn default_verify_every_days() -> u32 {
    7
}

fn is_default_verify_every_days(v: &u32) -> bool {
    *v == default_verify_every_days()
}

fn default_verify_read_percent() -> u32 {
    5
}

fn is_default_verify_read_percent(v: &u32) -> bool {
    *v == default_verify_read_percent()
}

pub fn default_notify_overdue_hours() -> u32 {
    48
}

fn is_default_notify_overdue_hours(v: &u32) -> bool {
    *v == default_notify_overdue_hours()
}

#[derive(Deserialize)]
#[serde(untagged)]
enum DailyAtInput {
    One(String),
    Many(Vec<String>),
}

fn deserialize_daily_at<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let opt = Option::<DailyAtInput>::deserialize(deserializer)?;
    Ok(match opt {
        None => Vec::new(),
        Some(DailyAtInput::One(s)) => {
            if s.trim().is_empty() {
                Vec::new()
            } else {
                vec![s]
            }
        }
        Some(DailyAtInput::Many(v)) => v.into_iter().filter(|s| !s.trim().is_empty()).collect(),
    })
}

fn default_interval_minutes() -> u64 {
    60
}

impl Default for ScheduleConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            interval_minutes: default_interval_minutes(),
            run_on_startup: false,
            auto_prune_after_backup: false,
            daily_at: Vec::new(),
            verify_every_days: default_verify_every_days(),
            verify_read_percent: default_verify_read_percent(),
            notify_overdue_hours: default_notify_overdue_hours(),
            maintenance_at: None,
        }
    }
}

impl ScheduleConfig {
    /// Sorted, deduplicated daily run times when `daily_at` is set. `load_or_default` rejects invalid values.
    pub fn daily_times(&self) -> Vec<NaiveTime> {
        let mut times: Vec<NaiveTime> = self
            .daily_at
            .iter()
            .filter_map(|s| parse_daily_at(s))
            .collect();
        times.sort();
        times.dedup();
        times
    }

    /// Parsed local maintenance slot time when `maintenance_at` is configured.
    pub fn maintenance_time(&self) -> Option<NaiveTime> {
        self.maintenance_at.as_deref().and_then(parse_daily_at)
    }
}

/// Normalizes an optional "HH:MM" maintenance time string; blank becomes `Ok(None)`.
pub fn normalize_maintenance_at(raw: Option<&str>) -> Option<Option<String>> {
    match raw.map(str::trim).filter(|s| !s.is_empty()) {
        None => Some(None),
        Some(s) => parse_daily_at(s).map(|t| Some(t.format("%H:%M").to_string())),
    }
}

/// Parses "HH:MM" (24-hour). Returns `None` for anything else.
pub fn parse_daily_at(s: &str) -> Option<NaiveTime> {
    NaiveTime::parse_from_str(s.trim(), "%H:%M").ok()
}

/// Parses, sorts, deduplicates, and formats a list of "HH:MM" (24-hour) strings.
pub fn normalize_daily_at<I, S>(items: I) -> Option<Vec<String>>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut times = Vec::new();
    for item in items {
        let s = item.as_ref().trim();
        if s.is_empty() {
            continue;
        }
        times.push(parse_daily_at(s)?);
    }
    times.sort();
    times.dedup();
    Some(
        times
            .into_iter()
            .map(|t| t.format("%H:%M").to_string())
            .collect(),
    )
}

/// Restic snapshot retention policy (`restic forget --prune`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RetentionPolicy {
    #[serde(default = "default_keep_last")]
    pub keep_last: u32,
    #[serde(default = "default_keep_daily")]
    pub keep_daily: u32,
    #[serde(default = "default_keep_weekly")]
    pub keep_weekly: u32,
    #[serde(default = "default_keep_monthly")]
    pub keep_monthly: u32,
}

fn default_keep_last() -> u32 {
    10
}
fn default_keep_daily() -> u32 {
    7
}
fn default_keep_weekly() -> u32 {
    4
}
fn default_keep_monthly() -> u32 {
    6
}

impl Default for RetentionPolicy {
    fn default() -> Self {
        Self {
            keep_last: default_keep_last(),
            keep_daily: default_keep_daily(),
            keep_weekly: default_keep_weekly(),
            keep_monthly: default_keep_monthly(),
        }
    }
}

fn is_false(v: &bool) -> bool {
    !*v
}

/// Security policy for the built-in MCP agent server (`--mcp`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AgentPolicy {
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Allows `restore_snapshot` via MCP into a separate validated target folder (default: true).
    #[serde(default = "default_true")]
    pub allow_restore: bool,
    /// Opt-in policy for snapshot pruning and prune-affecting settings
    /// (`forget_prune`, `update_retention_policy`, and schedule auto-prune / `maintenanceAt`
    /// changes) via MCP (default: false).
    #[serde(default)]
    pub allow_prune: bool,
    /// Opt-in policy for destructive R2 storage deletion (bucket or prefix)
    /// via MCP (default: false).
    #[serde(default, alias = "allowDeleteBucket", skip_serializing_if = "is_false")]
    pub allow_delete_storage: bool,
    /// Opt-in policy for overwriting/deleting local folders via MCP `takeover_project`
    /// (default: false).
    #[serde(default, skip_serializing_if = "is_false")]
    pub allow_handoff: bool,
}

impl Default for AgentPolicy {
    fn default() -> Self {
        Self {
            enabled: true,
            allow_restore: true,
            allow_prune: false,
            allow_delete_storage: false,
            allow_handoff: false,
        }
    }
}

/// Resolves the default application data directory under `%LOCALAPPDATA%\<APP_IDENTIFIER>`
/// (`%LOCALAPPDATA%\io.github.nayuengin.restic-tray`).
pub fn default_app_data_dir() -> PathBuf {
    if cfg!(windows) {
        app_data_dir_from_local_app_data(std::env::var_os("LOCALAPPDATA").as_deref())
    } else {
        app_data_dir_posix()
    }
}

fn app_data_dir_from_local_app_data(local_app_data: Option<&std::ffi::OsStr>) -> PathBuf {
    local_app_data
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join(APP_IDENTIFIER)
}

/// Per-OS application data directory on macOS / Linux:
/// - macOS: `$HOME/Library/Application Support`
/// - Linux: `$XDG_DATA_HOME` or `$HOME/.local/share`
///
/// Falls back to the current directory like the Windows path does without `LOCALAPPDATA`.
fn app_data_dir_posix() -> PathBuf {
    app_data_dir_posix_from(std::env::var_os("HOME"), std::env::var_os("XDG_DATA_HOME"))
}

fn app_data_dir_posix_from(
    home: Option<std::ffi::OsString>,
    xdg: Option<std::ffi::OsString>,
) -> PathBuf {
    let home = home.map(PathBuf::from);
    let base = if cfg!(target_os = "macos") {
        home.map(|h| h.join("Library").join("Application Support"))
    } else if let Some(xdg) = xdg.filter(|v| !v.is_empty()) {
        Some(PathBuf::from(xdg))
    } else {
        home.map(|h| h.join(".local").join("share"))
    };
    base.unwrap_or_else(|| PathBuf::from("."))
        .join(APP_IDENTIFIER)
}

pub fn default_config_path() -> PathBuf {
    default_app_data_dir().join(CONFIG_FILENAME)
}

/// The `config.pkl` evaluated (with pure-Rust `pklr`) instead of `config.json` when the latter
/// does not exist.
pub fn pkl_config_sibling(config_path: &Path) -> PathBuf {
    config_path.with_file_name(CONFIG_PKL_FILENAME)
}

impl AppConfig {
    /// Effective lowercase machine hostname used for `restic backup --host` and sync attribution.
    pub fn effective_machine_name(&self) -> String {
        self.machine_name
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| s.to_ascii_lowercase())
            .unwrap_or_else(default_os_machine_name)
    }

    pub fn has_shared_projects(&self) -> bool {
        self.targets.iter().any(|t| t.project_key.is_some())
    }

    pub fn has_active_backup_targets(&self) -> bool {
        self.targets.iter().any(BackupTarget::is_active_for_backup)
    }

    pub fn find_target_by_project_key(&self, key: &str) -> Option<&BackupTarget> {
        let needle = key.trim();
        self.targets
            .iter()
            .find(|t| t.project_key.as_deref() == Some(needle))
    }

    pub fn find_target_by_project_key_mut(&mut self, key: &str) -> Option<&mut BackupTarget> {
        let needle = key.trim();
        self.targets
            .iter_mut()
            .find(|t| t.project_key.as_deref() == Some(needle))
    }

    pub fn is_storage_configured(&self) -> bool {
        match self.storage_backend {
            StorageBackend::R2 => self.r2.is_configured(),
            StorageBackend::Rclone => self.rclone.is_configured(),
            StorageBackend::GoogleDrive => false,
        }
    }

    pub fn repository_url(&self) -> AppResult<String> {
        match self.storage_backend {
            StorageBackend::R2 => self.r2.repository_url(),
            StorageBackend::Rclone => self.rclone.repository_display(),
            StorageBackend::GoogleDrive => Err(AppError::Config(
                google_drive_removed_config_error(Path::new("config.json")),
            )),
        }
    }

    /// `--exclude` patterns for every target: the built-in set when enabled, then `globalExcludes`.
    pub fn shared_excludes(&self) -> Vec<&str> {
        let basic = if self.use_basic_excludes {
            BASIC_EXCLUDES
        } else {
            &[]
        };
        with_basic(basic, &self.global_excludes)
    }

    /// `--exclude-if-present` markers: the built-in set when enabled, then `excludeIfPresent`.
    pub fn exclude_markers(&self) -> Vec<&str> {
        let basic = if self.use_basic_excludes {
            BASIC_EXCLUDE_IF_PRESENT
        } else {
            &[]
        };
        with_basic(basic, &self.exclude_if_present)
    }
}

pub(crate) fn config_lock_path(path: &Path) -> PathBuf {
    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(CONFIG_FILENAME);
    path.with_file_name(format!("{file_name}.lock"))
}

/// Acquires a cross-process exclusive OS file lock adjacent to `config.json`.
/// Held until the returned `File` is dropped.
pub(crate) fn lock_config_exclusive(path: &Path) -> AppResult<File> {
    let lock_path = config_lock_path(path);
    if let Some(parent) = lock_path.parent() {
        fs::create_dir_all(parent)?;
    }
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)?;
    file.lock()?;
    Ok(file)
}

/// Acquires a cross-process shared OS file lock adjacent to `config.json`.
/// Held until the returned `File` is dropped.
pub(crate) fn lock_config_shared(path: &Path) -> AppResult<File> {
    let lock_path = config_lock_path(path);
    if let Some(parent) = lock_path.parent() {
        fs::create_dir_all(parent)?;
    }
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)?;
    file.lock_shared()?;
    Ok(file)
}

impl AppConfig {
    /// Validates and normalizes all mutable configuration fields.
    pub(crate) fn validate_and_normalize(&mut self, path: &Path) -> AppResult<()> {
        match self.storage_backend {
            StorageBackend::R2 => {}
            StorageBackend::Rclone => {
                self.rclone
                    .validate_remote()
                    .map_err(|e| AppError::Config(format!("rclone in {}: {e}", path.display())))?;
                if self.rclone.normalized_bucket().is_empty() {
                    return Err(AppError::Config(format!(
                        "rclone.bucket in {} must not be empty",
                        path.display()
                    )));
                }
            }
            StorageBackend::GoogleDrive => {
                return Err(AppError::Config(google_drive_removed_config_error(path)));
            }
        }
        self.ignore_files = self
            .ignore_files
            .iter()
            .map(|s| s.trim().replace('\\', "/"))
            .filter(|s| {
                if s.chars().any(|c| c.is_control()) || s.split('/').any(|seg| seg == "..") {
                    return false;
                }
                !s.is_empty()
            })
            .fold(Vec::new(), |mut acc: Vec<String>, s| {
                if !acc.contains(&s) {
                    acc.push(s);
                }
                acc
            });
        if !(MIN_INTERVAL_MINUTES..=MAX_INTERVAL_MINUTES).contains(&self.schedule.interval_minutes)
        {
            return Err(AppError::Config(format!(
                "schedule.intervalMinutes in {} must be between {} and {} (got {})",
                path.display(),
                MIN_INTERVAL_MINUTES,
                MAX_INTERVAL_MINUTES,
                self.schedule.interval_minutes
            )));
        }
        self.schedule.daily_at = normalize_daily_at(&self.schedule.daily_at).ok_or_else(|| {
            AppError::Config(format!(
                "schedule.dailyAt in {} must be \"HH:MM\" (24-hour)",
                path.display()
            ))
        })?;
        self.schedule.maintenance_at =
            normalize_maintenance_at(self.schedule.maintenance_at.as_deref()).ok_or_else(|| {
                AppError::Config(format!(
                    "schedule.maintenanceAt in {} must be \"HH:MM\" (24-hour)",
                    path.display()
                ))
            })?;
        if let Some(raw_name) = self.machine_name.take() {
            let trimmed = raw_name.trim();
            if !trimmed.is_empty() {
                self.machine_name = Some(normalize_machine_name(trimmed).map_err(|e| {
                    AppError::Config(format!("machineName in {}: {e}", path.display()))
                })?);
            }
        }
        if let Some(raw_root) = self.default_project_root.take() {
            let trimmed = raw_root.trim();
            if !trimmed.is_empty() {
                self.default_project_root = Some(trimmed.to_owned());
            }
        }
        let mut seen_keys = HashSet::new();
        for target in &mut self.targets {
            if let Some(raw_key) = target.project_key.take() {
                let trimmed = raw_key.trim();
                if !trimmed.is_empty() {
                    let valid = validate_project_key(trimmed).map_err(|e| {
                        AppError::Config(format!("targets[].projectKey in {}: {e}", path.display()))
                    })?;
                    if !seen_keys.insert(valid.clone()) {
                        return Err(AppError::Config(format!(
                            "duplicate projectKey \"{valid}\" in {}",
                            path.display()
                        )));
                    }
                    target.project_key = Some(valid);
                }
            }
            target.tags = validate_user_tags(&target.tags).map_err(|e| {
                AppError::Config(format!("targets[].tags in {}: {e}", path.display()))
            })?;
        }
        Ok(())
    }

    /// Loads `path` assuming the caller already holds the adjacent config lock file.
    /// When `config.json` is absent, a sibling `config.pkl` (Apple Pkl, evaluated with the pure-Rust
    /// `pklr` evaluator without network access) is read instead. Saving always writes `config.json`,
    /// which then takes precedence on the next load.
    pub(crate) fn load_or_default_unlocked(path: &Path) -> AppResult<Self> {
        if !path.exists() {
            let pkl_path = pkl_config_sibling(path);
            if pkl_path.exists() {
                let mut cfg = Self::load_pkl(&pkl_path)?;
                cfg.validate_and_normalize(path)?;
                return Ok(cfg);
            }
            let cfg = Self::default();
            cfg.save_atomic(path)?;
            return Ok(cfg);
        }
        let raw = fs::read_to_string(path)?;
        let mut cfg: Self = serde_json::from_str(&raw).map_err(|err| {
            AppError::Config(format!("failed to parse {}: {err}", path.display()))
        })?;
        cfg.validate_and_normalize(path)?;
        Ok(cfg)
    }

    /// Evaluates a Pkl configuration file to JSON with `pklr` (local files only; `http(s)` and
    /// `package://` imports are unavailable because network features are disabled).
    fn load_pkl(path: &Path) -> AppResult<Self> {
        let json = pklr::eval_to_json(path).map_err(|err| {
            AppError::Config(format!("failed to evaluate {}: {err}", path.display()))
        })?;
        serde_json::from_value(json)
            .map_err(|err| AppError::Config(format!("failed to parse {}: {err}", path.display())))
    }

    pub fn load_or_default(path: &Path) -> AppResult<Self> {
        if !path.exists() {
            let _lock = lock_config_exclusive(path)?;
            return Self::load_or_default_unlocked(path);
        }
        let _lock = lock_config_shared(path)?;
        Self::load_or_default_unlocked(path)
    }

    pub fn save_atomic(&self, path: &Path) -> AppResult<()> {
        static TMP_SEQ: AtomicU64 = AtomicU64::new(0);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_string_pretty(self)?;
        let seq = TMP_SEQ.fetch_add(1, Ordering::Relaxed);
        let file_name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(CONFIG_FILENAME);
        let tmp = path.with_file_name(format!("{file_name}.{}.{seq}.tmp", std::process::id()));
        if let Err(err) = fs::write(&tmp, json.as_bytes()) {
            let _ = fs::remove_file(&tmp);
            return Err(err.into());
        }
        if let Err(err) = fs::rename(&tmp, path) {
            let _ = fs::remove_file(&tmp);
            return Err(err.into());
        }
        Ok(())
    }
}

impl AppConfig {
    /// Add or update a backup target directory after verifying & canonicalizing its path.
    pub fn upsert_target(
        &mut self,
        path: &Path,
        label: Option<String>,
        exclude_patterns: Option<Vec<String>>,
        tags: Option<Vec<String>>,
        enabled: Option<bool>,
    ) -> AppResult<BackupTarget> {
        self.upsert_target_full(path, label, exclude_patterns, tags, enabled, None, None)
    }

    /// Add or update a backup target directory with optional multi-PC `projectKey` and `writerState`.
    #[allow(clippy::too_many_arguments)]
    pub fn upsert_target_full(
        &mut self,
        path: &Path,
        label: Option<String>,
        exclude_patterns: Option<Vec<String>>,
        tags: Option<Vec<String>>,
        enabled: Option<bool>,
        project_key: Option<Option<String>>,
        writer_state: Option<WriterState>,
    ) -> AppResult<BackupTarget> {
        let canonical = canonicalize_existing_dir(path, "backup target directory")?;
        let canonical_str = canonical.to_string_lossy().into_owned();

        let validated_key = match project_key {
            None => None,
            Some(None) => Some(None),
            Some(Some(raw)) => {
                let trimmed = raw.trim();
                if trimmed.is_empty() {
                    Some(None)
                } else {
                    let key = validate_project_key(trimmed)?;
                    if is_git_repository(&canonical) {
                        return Err(AppError::InvalidArgument(
                            "Git 管理リポジトリ（.git を含むフォルダ）には projectKey を設定できません。PC 間の受け渡しは GitHub を利用し、Restic Tray ではバックアップ専用にしてください"
                                .to_owned(),
                        ));
                    }
                    Some(Some(key))
                }
            }
        };

        let validated_tags = match tags {
            Some(tg) => Some(validate_user_tags(&tg)?),
            None => None,
        };

        let existing_idx = self
            .targets
            .iter()
            .position(|t| t.path.eq_ignore_ascii_case(&canonical_str));

        if let Some(Some(ref new_key)) = validated_key {
            let duplicate = self.targets.iter().enumerate().any(|(i, t)| {
                Some(i) != existing_idx && t.project_key.as_deref() == Some(new_key.as_str())
            });
            if duplicate {
                return Err(AppError::InvalidArgument(format!(
                    "projectKey \"{new_key}\" は既に別のターゲットで使用されています"
                )));
            }
        }

        if let Some(idx) = existing_idx {
            if let Some(requested_key) = validated_key {
                let current_key = self.targets[idx].project_key.as_deref();
                match (current_key, requested_key.as_deref()) {
                    (Some(existing_k), Some(new_k)) if existing_k != new_k => {
                        return Err(AppError::InvalidArgument(format!(
                            "確定済みの projectKey（{existing_k}）は履歴の連続性を守るため変更できません"
                        )));
                    }
                    (Some(existing_k), None) => {
                        return Err(AppError::InvalidArgument(format!(
                            "確定済みの projectKey（{existing_k}）は解除できません（解除が必要な場合は対象から外して再登録してください）"
                        )));
                    }
                    (None, Some(new_k)) => {
                        self.targets[idx].project_key = Some(new_k.to_owned());
                    }
                    _ => {}
                }
            }
            let existing = &mut self.targets[idx];
            if let Some(l) = label
                && !l.trim().is_empty()
            {
                existing.label = l;
            }
            if let Some(ex) = exclude_patterns {
                existing.exclude_patterns = ex;
            }
            if let Some(tg) = validated_tags {
                existing.tags = tg;
            }
            if let Some(en) = enabled {
                existing.enabled = en;
            }
            if let Some(ws) = writer_state {
                existing.writer_state = ws;
            }
            return Ok(existing.clone());
        }

        let mut target = BackupTarget::new(label.unwrap_or_default(), &canonical);
        if let Some(ex) = exclude_patterns {
            target.exclude_patterns = ex;
        }
        if let Some(tg) = validated_tags {
            target.tags = tg;
        }
        if let Some(en) = enabled {
            target.enabled = en;
        }
        if let Some(Some(key)) = validated_key {
            target.project_key = Some(key);
        }
        if let Some(ws) = writer_state {
            target.writer_state = ws;
        }
        self.targets.push(target.clone());
        Ok(target)
    }

    /// Registers or updates a restored shared project folder during takeover.
    pub fn register_shared_target(
        &mut self,
        canonical_path: &Path,
        label: Option<String>,
        project_key: &str,
        writer_state: WriterState,
    ) -> AppResult<BackupTarget> {
        let key = validate_project_key(project_key)?;
        let path_str = canonical_path.to_string_lossy().into_owned();
        if let Some(existing) = self
            .targets
            .iter_mut()
            .find(|t| t.project_key.as_deref() == Some(key.as_str()))
        {
            existing.path = path_str;
            existing.enabled = true;
            existing.writer_state = writer_state;
            if let Some(l) = label
                && !l.trim().is_empty()
            {
                existing.label = l;
            }
            return Ok(existing.clone());
        }
        self.upsert_target_full(
            canonical_path,
            label,
            None,
            None,
            Some(true),
            Some(Some(key)),
            Some(writer_state),
        )
    }

    /// Remove a target by its `id` or exact/canonical `path`.
    pub fn remove_target(&mut self, id_or_path: &str) -> bool {
        let needle = id_or_path.trim();
        let canonical_needle = dunce::canonicalize(Path::new(needle))
            .ok()
            .map(|p| p.to_string_lossy().into_owned());

        let before = self.targets.len();
        self.targets.retain(|t| {
            let matches_id = t.id == needle;
            let matches_raw_path = t.path.eq_ignore_ascii_case(needle);
            let matches_canonical = canonical_needle
                .as_deref()
                .is_some_and(|c| t.path.eq_ignore_ascii_case(c));
            !(matches_id || matches_raw_path || matches_canonical)
        });
        self.targets.len() != before
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn r2_repository_url_formats_standard_and_prefixed_buckets() {
        let mut r2 = R2Config {
            account_id: "acc12345".to_owned(),
            bucket: "my-backups".to_owned(),
            prefix: "workstation-01".to_owned(),
            ..Default::default()
        };
        assert_eq!(
            r2.repository_url().unwrap(),
            "s3:https://acc12345.r2.cloudflarestorage.com/my-backups/workstation-01"
        );

        r2.prefix.clear();
        assert_eq!(
            r2.repository_url().unwrap(),
            "s3:https://acc12345.r2.cloudflarestorage.com/my-backups"
        );
    }

    #[test]
    fn config_round_trip_and_target_upsert_remove() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cfg_path = dir.path().join("config.json");
        let target_dir = dir.path().join("docs-to-backup");
        fs::create_dir_all(&target_dir).expect("mkdir target");

        let mut cfg = AppConfig::load_or_default(&cfg_path).expect("init config");
        let added = cfg
            .upsert_target(
                &target_dir,
                Some("Documents".to_owned()),
                Some(vec!["*.bak".to_owned()]),
                Some(vec!["important".to_owned()]),
                Some(true),
            )
            .expect("upsert target");
        assert_eq!(added.label, "Documents");
        assert_eq!(cfg.targets.len(), 1);

        cfg.save_atomic(&cfg_path).expect("save config");
        let reloaded = AppConfig::load_or_default(&cfg_path).expect("reload config");
        assert_eq!(reloaded.targets.len(), 1);
        assert_eq!(reloaded.targets[0].exclude_patterns, vec!["*.bak"]);

        let mut reloaded = reloaded;
        assert!(reloaded.remove_target(&added.id));
        assert!(reloaded.targets.is_empty());
    }

    #[test]
    fn config_from_an_older_version_applies_basic_excludes_before_its_own() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cfg_path = dir.path().join("config.json");
        fs::write(&cfg_path, r#"{"globalExcludes": ["*.iso"]}"#).unwrap();

        let cfg = AppConfig::load_or_default(&cfg_path).unwrap();
        let excludes = cfg.shared_excludes();
        assert!(cfg.use_basic_excludes);
        assert_eq!(&excludes[..BASIC_EXCLUDES.len()], BASIC_EXCLUDES);
        assert_eq!(excludes.last(), Some(&"*.iso"));
        assert_eq!(cfg.exclude_markers(), BASIC_EXCLUDE_IF_PRESENT);
    }

    #[test]
    fn schedule_daily_at_accepts_single_string_or_array_and_normalizes() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cfg_path = dir.path().join("config.json");

        fs::write(&cfg_path, r#"{"schedule": {"dailyAt": "05:00"}}"#).unwrap();
        let cfg = AppConfig::load_or_default(&cfg_path).unwrap();
        assert_eq!(cfg.schedule.daily_at, vec!["05:00"]);

        fs::write(
            &cfg_path,
            r#"{"schedule": {"dailyAt": ["17:00", "5:00", "17:00"]}}"#,
        )
        .unwrap();
        let cfg = AppConfig::load_or_default(&cfg_path).unwrap();
        assert_eq!(cfg.schedule.daily_at, vec!["05:00", "17:00"]);
    }

    #[test]
    fn legacy_config_defaults_to_r2_and_omits_new_default_keys() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cfg_path = dir.path().join("config.json");
        fs::write(&cfg_path, r#"{"r2":{"accountId":"acc","bucket":"bkt"}}"#).unwrap();

        let cfg = AppConfig::load_or_default(&cfg_path).unwrap();
        assert!(cfg.is_storage_configured());

        let default_json = serde_json::to_string(&AppConfig::default()).unwrap();
        assert!(!default_json.contains("storageBackend"));
        assert!(!default_json.contains("googleDrive"));
        assert!(!default_json.contains("rcloneBinaryPath"));
        assert!(!default_json.contains("allowDeleteStorage"));
        assert!(!default_json.contains("allowDeleteBucket"));
        assert!(!default_json.contains("allowHandoff"));
        assert!(!default_json.contains("machineName"));
        assert!(!default_json.contains("defaultProjectRoot"));
        assert!(!default_json.contains("maintenanceRole"));
        assert!(!default_json.contains("maintenanceAt"));
        assert!(!cfg.agent.allow_prune);
        assert!(cfg.agent.allow_restore);
        assert!(!cfg.agent.allow_delete_storage);
        assert!(!cfg.agent.allow_handoff);
        assert_eq!(cfg.maintenance_role, MaintenanceRole::Primary);

        // `allowDeleteBucket` alias also enables `allow_delete_storage`.
        let alias_cfg: AppConfig =
            serde_json::from_str(r#"{"agent":{"allowDeleteBucket":true}}"#).unwrap();
        assert!(alias_cfg.agent.allow_delete_storage);
    }

    #[test]
    fn multi_pc_config_fields_round_trip_and_validate() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cfg_path = dir.path().join("config.json");
        let shared_dir = dir.path().join("ProjectAlpha");
        let git_dir = dir.path().join("GitProj");
        fs::create_dir_all(&shared_dir).unwrap();
        fs::create_dir_all(git_dir.join(".git")).unwrap();

        let mut cfg = AppConfig {
            machine_name: Some("PC-B".to_owned()),
            default_project_root: Some(r"E:\Projects".to_owned()),
            maintenance_role: MaintenanceRole::None,
            schedule: ScheduleConfig {
                maintenance_at: Some("5:00".to_owned()),
                ..Default::default()
            },
            agent: AgentPolicy {
                allow_handoff: true,
                ..Default::default()
            },
            ..Default::default()
        };
        let target = cfg
            .upsert_target_full(
                &shared_dir,
                Some("ProjectAlpha".to_owned()),
                None,
                None,
                Some(true),
                Some(Some("project-alpha".to_owned())),
                Some(WriterState::Standby),
            )
            .expect("shared target");
        assert_eq!(target.project_key.as_deref(), Some("project-alpha"));
        assert_eq!(target.writer_state, WriterState::Standby);
        assert!(!target.is_active_for_backup());

        // Git repo cannot be assigned a projectKey.
        let err = cfg
            .upsert_target_full(
                &git_dir,
                None,
                None,
                None,
                Some(true),
                Some(Some("git-proj".to_owned())),
                None,
            )
            .unwrap_err();
        assert!(err.to_string().contains("Git"));

        // Immutable once set.
        let err = cfg
            .upsert_target_full(
                &shared_dir,
                None,
                None,
                None,
                None,
                Some(Some("other-key".to_owned())),
                None,
            )
            .unwrap_err();
        assert!(err.to_string().contains("変更できません"));

        cfg.save_atomic(&cfg_path).unwrap();
        let reloaded = AppConfig::load_or_default(&cfg_path).unwrap();
        assert_eq!(reloaded.effective_machine_name(), "pc-b");
        assert_eq!(reloaded.maintenance_role, MaintenanceRole::None);
        assert_eq!(reloaded.schedule.maintenance_at.as_deref(), Some("05:00"));
        assert!(reloaded.agent.allow_handoff);
        assert_eq!(
            reloaded.targets[0].project_key.as_deref(),
            Some("project-alpha")
        );
        assert_eq!(reloaded.targets[0].writer_state, WriterState::Standby);
    }

    #[test]
    fn project_key_validation_and_suggestion() {
        assert_eq!(
            validate_project_key("project-alpha").unwrap(),
            "project-alpha"
        );
        assert_eq!(
            validate_project_key("sample-asset_kit.2").unwrap(),
            "sample-asset_kit.2"
        );
        assert!(validate_project_key("ProjectAlpha").is_err());
        assert!(validate_project_key("-bad").is_err());
        assert!(validate_project_key("").is_err());
        assert!(validate_project_key(&"a".repeat(65)).is_err());

        assert_eq!(suggest_project_key("SampleProj"), "sampleproj");
        assert_eq!(suggest_project_key("Sample Asset Kit!"), "sample-asset-kit");
        assert_eq!(suggest_project_key("日本語のみ"), "project");
    }

    #[test]
    fn r2_endpoint_url_validates_account_id_and_respects_override() {
        let mut r2 = R2Config {
            account_id: "acc12345".to_owned(),
            bucket: "bkt".to_owned(),
            ..Default::default()
        };
        assert_eq!(
            r2.endpoint_url().unwrap(),
            "https://acc12345.r2.cloudflarestorage.com"
        );
        r2.endpoint_override = Some("http://127.0.0.1:9000/".to_owned());
        assert_eq!(r2.endpoint_url().unwrap(), "http://127.0.0.1:9000");

        r2.endpoint_override = None;
        r2.account_id = "bad/account".to_owned();
        assert!(r2.endpoint_url().is_err());
    }

    #[test]
    fn legacy_google_drive_keys_load_and_drop_on_resave() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cfg_path = dir.path().join("config.json");
        let legacy_raw = r#"{
  "r2": { "accountId": "acc123", "bucket": "my-bucket", "prefix": "pc-01" },
  "googleDrive": {
    "folder": "restic-tray",
    "scope": "drive.file",
    "rootFolderId": "folder123",
    "teamDriveId": "team456",
    "clientId": "cid.apps.googleusercontent.com",
    "maxConnections": 5
  },
  "rcloneBinaryPath": "C:\\tools\\rclone.exe"
}"#;
        fs::write(&cfg_path, legacy_raw).unwrap();

        let cfg = AppConfig::load_or_default(&cfg_path).expect("load legacy config");
        assert!(cfg.is_storage_configured());
        assert_eq!(
            cfg.repository_url().unwrap(),
            "s3:https://acc123.r2.cloudflarestorage.com/my-bucket/pc-01"
        );
        // Loading alone must not rewrite config.json.
        let disk_after_load = fs::read_to_string(&cfg_path).unwrap();
        assert_eq!(disk_after_load, legacy_raw);

        // Saving again omits deprecated keys.
        cfg.save_atomic(&cfg_path).unwrap();
        let resaved = fs::read_to_string(&cfg_path).unwrap();
        assert!(!resaved.contains("googleDrive"));
        assert!(!resaved.contains("rcloneBinaryPath"));
        assert!(!resaved.contains("storageBackend"));

        // Explicit `"storageBackend": "r2"` also loads and is omitted on re-save.
        fs::write(
            &cfg_path,
            r#"{"storageBackend":"r2","r2":{"accountId":"acc123","bucket":"my-bucket"}}"#,
        )
        .unwrap();
        let cfg_r2 = AppConfig::load_or_default(&cfg_path).expect("load explicit r2");
        assert!(cfg_r2.is_storage_configured());
        cfg_r2.save_atomic(&cfg_path).unwrap();
        let resaved_r2 = fs::read_to_string(&cfg_path).unwrap();
        assert!(!resaved_r2.contains("storageBackend"));
    }

    #[test]
    fn storage_backend_google_drive_returns_dedicated_removal_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cfg_path = dir.path().join("config.json");
        let gdrive_raw =
            r#"{"storageBackend":"googleDrive","googleDrive":{"folder":"restic-tray"}}"#;
        fs::write(&cfg_path, gdrive_raw).unwrap();

        let err = AppConfig::load_or_default(&cfg_path).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("Cloudflare R2"), "unexpected error: {msg}");
        assert!(msg.contains("storageBackend"), "unexpected error: {msg}");
        assert!(
            !msg.contains("v0.7.0"),
            "public error must not advise downgrading to unpublished v0.7.0: {msg}"
        );
        assert!(
            msg.contains("restic") && msg.contains("rclone"),
            "unexpected error: {msg}"
        );
        assert_eq!(fs::read_to_string(&cfg_path).unwrap(), gdrive_raw);

        // Explicit `"storageBackend": null` must fail cleanly without modifying the file.
        let null_raw = r#"{"storageBackend":null,"r2":{"accountId":"acc","bucket":"bkt"}}"#;
        fs::write(&cfg_path, null_raw).unwrap();
        assert!(AppConfig::load_or_default(&cfg_path).is_err());
        assert_eq!(fs::read_to_string(&cfg_path).unwrap(), null_raw);

        // Unknown non-r2 backend also fails cleanly.
        fs::write(&cfg_path, r#"{"storageBackend":"s3"}"#).unwrap();
        assert!(AppConfig::load_or_default(&cfg_path).is_err());
    }

    #[test]
    fn partial_schedule_defaults_auto_prune_to_false_consistently() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cfg_path = dir.path().join("config.json");

        // 1. Missing schedule section
        fs::write(&cfg_path, r#"{}"#).unwrap();
        let missing = AppConfig::load_or_default(&cfg_path).unwrap();
        assert!(!missing.schedule.auto_prune_after_backup);

        // 2. Empty schedule object
        fs::write(&cfg_path, r#"{"schedule": {}}"#).unwrap();
        let empty = AppConfig::load_or_default(&cfg_path).unwrap();
        assert!(!empty.schedule.auto_prune_after_backup);

        // 3. Interval-only schedule object
        fs::write(&cfg_path, r#"{"schedule": {"intervalMinutes": 120}}"#).unwrap();
        let interval_only = AppConfig::load_or_default(&cfg_path).unwrap();
        assert_eq!(interval_only.schedule.interval_minutes, 120);
        assert!(!interval_only.schedule.auto_prune_after_backup);

        // 4. Explicitly enabled
        fs::write(
            &cfg_path,
            r#"{"schedule": {"intervalMinutes": 120, "autoPruneAfterBackup": true}}"#,
        )
        .unwrap();
        let explicit = AppConfig::load_or_default(&cfg_path).unwrap();
        assert!(explicit.schedule.auto_prune_after_backup);
    }

    #[test]
    fn load_validates_schedule_interval_minutes_bounds() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cfg_path = dir.path().join("config.json");

        for valid in [MIN_INTERVAL_MINUTES, 60, MAX_INTERVAL_MINUTES] {
            fs::write(
                &cfg_path,
                format!(r#"{{"schedule":{{"intervalMinutes":{valid}}}}}"#),
            )
            .unwrap();
            let cfg = AppConfig::load_or_default(&cfg_path).unwrap();
            assert_eq!(cfg.schedule.interval_minutes, valid);
        }

        for invalid in [0_u64, MAX_INTERVAL_MINUTES + 1, u64::MAX] {
            fs::write(
                &cfg_path,
                format!(r#"{{"schedule":{{"intervalMinutes":{invalid}}}}}"#),
            )
            .unwrap();
            let err = AppConfig::load_or_default(&cfg_path).unwrap_err();
            assert!(
                err.to_string().contains("intervalMinutes"),
                "expected intervalMinutes error for {invalid}, got: {err}"
            );
        }
    }

    #[test]
    fn app_identifier_and_default_data_paths_use_expected_id() {
        use std::ffi::OsStr;

        assert_eq!(APP_IDENTIFIER, "io.github.nayuengin.restic-tray");

        let resolved =
            app_data_dir_from_local_app_data(Some(OsStr::new(r"C:\Users\example\AppData\Local")));
        assert_eq!(
            resolved,
            PathBuf::from(r"C:\Users\example\AppData\Local\io.github.nayuengin.restic-tray")
        );

        let fallback = app_data_dir_from_local_app_data(None);
        assert_eq!(
            fallback,
            PathBuf::from(".").join("io.github.nayuengin.restic-tray")
        );

        let default_dir = default_app_data_dir();
        assert_eq!(default_dir.file_name(), Some(OsStr::new(APP_IDENTIFIER)));
        assert_eq!(default_config_path(), default_dir.join(CONFIG_FILENAME));
    }

    #[test]
    fn agent_policy_defaults_disable_prune_and_allow_restore() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cfg_path = dir.path().join("config.json");

        let default_cfg = AppConfig::default();
        assert!(default_cfg.agent.enabled);
        assert!(default_cfg.agent.allow_restore);
        assert!(!default_cfg.agent.allow_prune);
        assert!(!default_cfg.agent.allow_delete_storage);
        assert!(!default_cfg.agent.allow_handoff);

        // Saving default config explicitly writes `"allowPrune": false`.
        default_cfg.save_atomic(&cfg_path).unwrap();
        let saved_raw = fs::read_to_string(&cfg_path).unwrap();
        assert!(saved_raw.contains("\"allowPrune\": false"));

        // Missing agent section or missing allowPrune key defaults to false.
        for raw in [r#"{}"#, r#"{"agent":{}}"#] {
            fs::write(&cfg_path, raw).unwrap();
            let loaded = AppConfig::load_or_default(&cfg_path).unwrap();
            assert!(loaded.agent.enabled);
            assert!(loaded.agent.allow_restore);
            assert!(!loaded.agent.allow_prune);
        }

        // Existing configs that explicitly persisted `"allowPrune": true` keep it enabled.
        fs::write(&cfg_path, r#"{"agent":{"allowPrune":true}}"#).unwrap();
        let explicit = AppConfig::load_or_default(&cfg_path).unwrap();
        assert!(explicit.agent.allow_prune);
        assert!(explicit.agent.allow_restore);
    }
    #[test]
    fn app_data_dir_posix_uses_xdg_and_home() {
        let home = Some(std::ffi::OsString::from("/home/tester"));
        let xdg = Some(std::ffi::OsString::from("/custom/data"));
        let with_xdg = app_data_dir_posix_from(home.clone(), xdg);
        assert!(with_xdg.ends_with(Path::new(APP_IDENTIFIER)));
        if cfg!(target_os = "macos") {
            assert!(with_xdg.starts_with(Path::new("/home/tester/Library/Application Support")));
        } else {
            assert_eq!(with_xdg, PathBuf::from("/custom/data").join(APP_IDENTIFIER));
            let home_only = app_data_dir_posix_from(home, None);
            assert_eq!(
                home_only,
                PathBuf::from("/home/tester/.local/share").join(APP_IDENTIFIER)
            );
        }
        assert_eq!(
            app_data_dir_posix_from(None, None),
            PathBuf::from(".").join(APP_IDENTIFIER)
        );
    }

    #[test]
    fn rclone_backend_loads_validates_and_round_trips() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cfg_path = dir.path().join("config.json");
        fs::write(
            &cfg_path,
            r#"{"storageBackend":"rclone","rclone":{"remote":"mycrypt:","bucket":"restic"}}"#,
        )
        .unwrap();
        let cfg = AppConfig::load_or_default(&cfg_path).unwrap();
        assert_eq!(cfg.storage_backend, StorageBackend::Rclone);
        assert!(cfg.is_storage_configured());
        assert!(
            cfg.repository_url()
                .unwrap()
                .contains("rclone serve s3 -> mycrypt: (bucket: restic)")
        );
        cfg.save_atomic(&cfg_path).unwrap();
        let resaved = fs::read_to_string(&cfg_path).unwrap();
        assert!(resaved.contains("\"storageBackend\": \"rclone\""));
        assert!(resaved.contains("\"remote\": \"mycrypt:\""));
        // Default bucket name round-trips without an explicit value.
        assert!(resaved.contains("\"bucket\": \"restic\""));
    }

    #[test]
    fn rclone_backend_rejects_invalid_remotes() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cfg_path = dir.path().join("config.json");
        for bad in ["", "nosemicolon", ":path", "my crypt:x", "crypt:a/../b"] {
            fs::write(
                &cfg_path,
                format!(r#"{{"storageBackend":"rclone","rclone":{{"remote":"{bad}"}}}}"#),
            )
            .unwrap();
            let err = AppConfig::load_or_default(&cfg_path).unwrap_err();
            assert!(err.to_string().contains("rclone"), "remote {bad:?}: {err}");
        }
    }

    #[test]
    fn rclone_backend_requires_bucket() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cfg_path = dir.path().join("config.json");
        fs::write(
            &cfg_path,
            r#"{"storageBackend":"rclone","rclone":{"remote":"mycrypt:","bucket":"  "}}"#,
        )
        .unwrap();
        assert!(AppConfig::load_or_default(&cfg_path).is_err());
    }

    #[test]
    fn rclone_normalized_remote_accepts_root_and_subpaths() {
        let mut rclone = RcloneConfig {
            remote: "MyCrypt:Sub/dir/".to_owned(),
            ..Default::default()
        };
        assert_eq!(rclone.normalized_remote(), "MyCrypt:Sub/dir");
        assert!(rclone.validate_remote().is_ok());
        rclone.remote = "crypt:".to_owned();
        assert_eq!(rclone.normalized_remote(), "crypt:");
        assert!(rclone.validate_remote().is_ok());
        rclone.remote = r"local:D:\backups".to_owned();
        assert_eq!(rclone.normalized_remote(), "local:D:/backups");
        assert!(rclone.validate_remote().is_ok());
    }

    #[test]
    fn ignore_settings_normalize_on_load() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cfg_path = dir.path().join("config.json");
        fs::write(
            &cfg_path,
            r#"{"useGitignore":true,"ignoreFiles":[" a/.resticignore ","", "b/../c", "dup", "dup"]}"#,
        )
        .unwrap();
        let cfg = AppConfig::load_or_default(&cfg_path).unwrap();
        assert!(cfg.use_gitignore);
        assert_eq!(cfg.ignore_files, vec!["a/.resticignore", "dup"]);
    }

    #[test]
    fn config_pkl_is_evaluated_when_json_is_absent() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cfg_path = dir.path().join("config.json");
        let pkl_path = pkl_config_sibling(&cfg_path);
        fs::write(
            &pkl_path,
            "schedule {\n  enabled = true\n  intervalMinutes = 90\n}\nr2 {\n  accountId = \"acc123\"\n  bucket = \"bkt\"\n}",
        )
        .unwrap();
        let cfg = AppConfig::load_or_default(&cfg_path).unwrap();
        assert!(cfg.schedule.enabled);
        assert_eq!(cfg.schedule.interval_minutes, 90);
        assert_eq!(cfg.r2.account_id, "acc123");
        // Saving writes config.json, which then takes precedence.
        cfg.save_atomic(&cfg_path).unwrap();
        assert!(cfg_path.exists());
    }

    #[test]
    fn config_pkl_invalid_file_fails_cleanly() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cfg_path = dir.path().join("config.json");
        let pkl_path = pkl_config_sibling(&cfg_path);
        fs::write(&pkl_path, "r2 { accountId = ").unwrap();
        assert!(AppConfig::load_or_default(&cfg_path).is_err());
        assert!(!cfg_path.exists());
    }
}
