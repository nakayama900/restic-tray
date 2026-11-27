//! Pure-Rust (`eframe`/`egui` 0.37) GUI for Restic Tray.
//!
//! A fixed sidebar (navigation and next-run status) and one page at a time:
//! 1. Overview: protection status, the backup action, 7-day coverage, recent runs
//! 2. Targets: backup folders (also by drag and drop), per-folder and shared excludes
//! 3. Snapshots: repository snapshot browser, file listing, diff, confirmed restore
//! 4. History: backup runs and the operation log (GUI and MCP)
//! 5. Settings: Cloudflare R2 storage, DPAPI credentials, schedule, retention, MCP, app behavior

mod format;
mod history;
mod overview;
mod settings;
mod snapshots;
mod targets;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::{Duration, Instant};

use chrono::Timelike;

use egui::{
    Align, Align2, Color32, Context, CornerRadius, Frame, Id, Key, LayerId, Layout, Margin,
    Modifiers, Order, Pos2, Rect, RichText, Sense, Shape, Stroke, Vec2,
};
use zeroize::Zeroize;

use crate::config::{AppConfig, MaintenanceRole, WriterState};
use crate::controller::{AppStatusReport, SharedController};
use crate::db::{AuditLogRecord, BackupRunRecord, HistoryDb};
use crate::r2_s3::{R2DeleteReport, R2DeleteScope};
use crate::restic::runner;
use crate::restic::types::{FolderBytes, ResticDiffReport, ResticLsEntry, ResticSnapshot};
use crate::sync::{ProjectSyncReport, TakeoverParams, TakeoverReport};
use crate::theme::{
    self, Btn, DesignTokens, NavTrailing, VectorIcon, body, field_label, help_text,
    hold_danger_button, icon_button, input, mono, nav_item, paint_brand_mark, paint_icon,
    paint_spinner, paint_status_dot, progress_bar, strong,
};
use crate::tray::SystemTrayHandle;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppTab {
    Dashboard,
    Targets,
    Snapshots,
    History,
    Settings,
}

/// Long-running work started from the GUI. Tracked so buttons can show a spinner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Job {
    Backup,
    DryRun,
    Restore,
    InitRepo,
    CheckRepo,
    DeleteR2,
    ReleaseProject,
    TakeoverProject,
    UnlockRepo,
    ForgetPrune,
    ForgetSnapshot,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SnapshotOp {
    List,
    Files,
    Diff,
}

enum UiWorkerEvent {
    StatusRefreshed {
        report: Box<AppStatusReport>,
        runs: Vec<BackupRunRecord>,
        audits: Vec<AuditLogRecord>,
        sync_report: Option<ProjectSyncReport>,
    },
    SyncReportLoaded(Result<ProjectSyncReport, String>),
    TakeoverPreviewLoaded(u64, Result<TakeoverReport, String>),
    TakeoverFinished(Result<TakeoverReport, String>),
    ForgetPrunePreviewLoaded(Result<String, String>),
    SnapshotsLoaded(Result<Vec<ResticSnapshot>, String>),
    SnapshotFilesLoaded(Result<(String, Vec<ResticLsEntry>), String>),
    DiffLoaded(Result<ResticDiffReport, String>),
    SizesLoaded(Result<Vec<FolderBytes>, String>),
    R2DeletePreviewLoaded(Result<R2DeleteReport, String>),
    R2DeleteFinished(Result<R2DeleteReport, String>),
    JobFinished(Job, Result<String, String>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ToastKind {
    Success,
    Error,
}

struct Toast {
    id: u64,
    message: String,
    kind: ToastKind,
    born: f64,
}

/// Actions that need an explicit second step because they delete, overwrite or stop things.
#[derive(Clone)]
enum Confirm {
    RemoveTarget {
        id: String,
        label: String,
    },
    Restore {
        snapshot_id: String,
        snapshot_label: String,
        target_dir: String,
        includes: Vec<String>,
    },
    InitRepo,
    DeleteR2 {
        scope: R2DeleteScope,
        bucket: String,
        prefix: String,
        endpoint: String,
    },
    ReleaseProject {
        project_key: String,
        label: String,
        path: String,
    },
    TakeoverProject {
        project_key: String,
        label: String,
        local_path: String,
        delete_extra: bool,
        save_local_backup_first: bool,
        discard_confirmed: bool,
        force_while_remote_active: bool,
    },
    ForgetPrune {
        dry_run: bool,
        non_primary_warning: bool,
    },
    ForgetSnapshot {
        snapshot_id: String,
        snapshot_label: String,
    },
    UnlockRepo,
    Quit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HistoryView {
    Runs,
    Operations,
}

/// Editable copy of the non-secret settings. Compared with the saved config to show the save bar.
#[derive(Debug, Clone, PartialEq, Eq)]
struct SettingsForm {
    machine_name: String,
    default_project_root: String,
    maintenance_role: MaintenanceRole,
    account_id: String,
    bucket: String,
    prefix: String,
    endpoint: String,
    region: String,
    max_connections: u32,
    restic_path: String,
    backend_rclone: bool,
    rclone_remote: String,
    rclone_bucket: String,
    rclone_port: String,
    rclone_path: String,
    rclone_config: String,
    use_gitignore: bool,
    /// One ignore file per line (`ignoreFiles`).
    ignore_files: String,
    schedule_enabled: bool,
    interval_minutes: u64,
    daily: bool,
    daily_times: Vec<(u32, u32)>,
    run_on_startup: bool,
    auto_prune: bool,
    maintenance_slot_enabled: bool,
    maintenance_hour: u32,
    maintenance_minute: u32,
    verify_enabled: bool,
    notify_enabled: bool,
    /// Not edited here; the values the two switches above turn on (set in `config.json`).
    verify_every_days: u32,
    verify_read_percent: u32,
    notify_overdue_hours: u32,
    keep_last: u32,
    keep_daily: u32,
    keep_weekly: u32,
    keep_monthly: u32,
    agent_enabled: bool,
    allow_restore: bool,
    allow_prune: bool,
    allow_delete_storage: bool,
    allow_handoff: bool,
    start_minimized: bool,
}

impl SettingsForm {
    fn from_config(cfg: &AppConfig) -> Self {
        let parsed = cfg.schedule.daily_times();
        let daily = !parsed.is_empty();
        let daily_times = if daily {
            parsed.into_iter().map(|t| (t.hour(), t.minute())).collect()
        } else {
            vec![(5, 0)]
        };
        let maint = cfg.schedule.maintenance_time();
        let (maintenance_slot_enabled, maintenance_hour, maintenance_minute) = match maint {
            Some(t) => (true, t.hour(), t.minute()),
            None => (false, 5, 0),
        };
        Self {
            machine_name: cfg.machine_name.clone().unwrap_or_default(),
            default_project_root: cfg.default_project_root.clone().unwrap_or_default(),
            maintenance_role: cfg.maintenance_role,
            account_id: cfg.r2.account_id.clone(),
            bucket: cfg.r2.bucket.clone(),
            prefix: cfg.r2.prefix.clone(),
            endpoint: cfg.r2.endpoint_override.clone().unwrap_or_default(),
            region: cfg.r2.region.clone(),
            max_connections: cfg.r2.max_connections,
            restic_path: cfg.restic_binary_path.clone().unwrap_or_default(),
            backend_rclone: cfg.storage_backend == crate::config::StorageBackend::Rclone,
            rclone_remote: cfg.rclone.remote.clone(),
            rclone_bucket: cfg.rclone.bucket.clone(),
            rclone_port: cfg.rclone.port.map(|p| p.to_string()).unwrap_or_default(),
            rclone_path: cfg.rclone.binary_path.clone().unwrap_or_default(),
            rclone_config: cfg.rclone.config_path.clone().unwrap_or_default(),
            use_gitignore: cfg.use_gitignore,
            ignore_files: cfg.ignore_files.join("\n"),
            schedule_enabled: cfg.schedule.enabled,
            interval_minutes: cfg.schedule.interval_minutes,
            daily,
            daily_times,
            run_on_startup: cfg.schedule.run_on_startup,
            auto_prune: cfg.schedule.auto_prune_after_backup,
            maintenance_slot_enabled,
            maintenance_hour,
            maintenance_minute,
            verify_enabled: cfg.schedule.verify_every_days > 0,
            notify_enabled: cfg.schedule.notify_overdue_hours > 0,
            verify_every_days: Some(cfg.schedule.verify_every_days)
                .filter(|d| *d > 0)
                .unwrap_or_else(crate::config::default_verify_every_days),
            verify_read_percent: cfg.schedule.verify_read_percent,
            notify_overdue_hours: Some(cfg.schedule.notify_overdue_hours)
                .filter(|h| *h > 0)
                .unwrap_or_else(crate::config::default_notify_overdue_hours),
            keep_last: cfg.retention.keep_last,
            keep_daily: cfg.retention.keep_daily,
            keep_weekly: cfg.retention.keep_weekly,
            keep_monthly: cfg.retention.keep_monthly,
            agent_enabled: cfg.agent.enabled,
            allow_restore: cfg.agent.allow_restore,
            allow_prune: cfg.agent.allow_prune,
            allow_delete_storage: cfg.agent.allow_delete_storage,
            allow_handoff: cfg.agent.allow_handoff,
            start_minimized: cfg.start_minimized_to_tray,
        }
    }

    fn r2_matches_saved(&self, cfg: &AppConfig) -> bool {
        let opt = |s: &str| {
            let s = s.trim();
            (!s.is_empty()).then(|| s.to_owned())
        };
        let saved_region = {
            let r = cfg.r2.region.trim();
            if r.is_empty() {
                "auto".to_owned()
            } else {
                r.to_owned()
            }
        };
        self.account_id.trim() == cfg.r2.account_id.trim()
            && self.bucket.trim() == cfg.r2.bucket.trim()
            && self.prefix.trim() == cfg.r2.prefix.trim()
            && opt(&self.endpoint) == cfg.r2.endpoint_override.as_deref().and_then(opt)
            && opt(&self.region).unwrap_or_else(|| "auto".to_owned()) == saved_region
    }

    fn validate(&self) -> crate::error::AppResult<()> {
        let name = self.machine_name.trim();
        if !name.is_empty() {
            let _ = crate::config::normalize_machine_name(name)?;
        }
        if self.backend_rclone {
            let rclone = crate::config::RcloneConfig {
                remote: self.rclone_remote.clone(),
                bucket: self.rclone_bucket.clone(),
                ..Default::default()
            };
            rclone.validate_remote()?;
        }
        for entry in self.ignore_files.lines() {
            let entry = entry.trim();
            let invalid = !entry.is_empty()
                && (entry.chars().any(|c| c.is_control())
                    || entry.split(['/', '\\']).any(|seg| seg == ".."));
            if invalid {
                return Err(crate::error::AppError::Config(format!(
                    "ignoreFiles に使えないパスが含まれています: {entry}"
                )));
            }
        }
        Ok(())
    }

    fn apply(&self, cfg: &mut AppConfig) {
        let optional = |s: &str| {
            let s = s.trim();
            (!s.is_empty()).then(|| s.to_owned())
        };
        cfg.machine_name = optional(&self.machine_name).map(|s| s.to_ascii_lowercase());
        cfg.default_project_root = optional(&self.default_project_root);
        cfg.maintenance_role = self.maintenance_role;
        cfg.r2.account_id = self.account_id.trim().to_owned();
        cfg.r2.bucket = self.bucket.trim().to_owned();
        cfg.r2.prefix = self.prefix.trim().to_owned();
        cfg.r2.endpoint_override = optional(&self.endpoint);
        cfg.r2.region = optional(&self.region).unwrap_or_else(|| "auto".to_owned());
        cfg.r2.max_connections = self.max_connections;
        cfg.restic_binary_path = optional(&self.restic_path);
        cfg.storage_backend = if self.backend_rclone {
            crate::config::StorageBackend::Rclone
        } else {
            crate::config::StorageBackend::R2
        };
        cfg.rclone.remote = self.rclone_remote.trim().to_owned();
        cfg.rclone.bucket = if self.rclone_bucket.trim().is_empty() {
            "restic".to_owned()
        } else {
            self.rclone_bucket.trim().to_owned()
        };
        cfg.rclone.port = self
            .rclone_port
            .trim()
            .parse::<u16>()
            .ok()
            .filter(|p| *p > 0);
        cfg.rclone.binary_path = optional(&self.rclone_path);
        cfg.rclone.config_path = optional(&self.rclone_config);
        cfg.use_gitignore = self.use_gitignore;
        cfg.ignore_files = self
            .ignore_files
            .lines()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .collect();
        cfg.schedule.enabled = self.schedule_enabled;
        cfg.schedule.interval_minutes = self.interval_minutes.max(1);
        cfg.schedule.daily_at = if self.daily {
            let mut list: Vec<String> = self
                .daily_times
                .iter()
                .map(|&(h, m)| format!("{:02}:{:02}", h.min(23), m.min(59)))
                .collect();
            list.sort();
            list.dedup();
            if list.is_empty() {
                vec!["05:00".to_owned()]
            } else {
                list
            }
        } else {
            Vec::new()
        };
        cfg.schedule.run_on_startup = self.run_on_startup;
        cfg.schedule.auto_prune_after_backup = self.auto_prune;
        cfg.schedule.maintenance_at = if self.maintenance_slot_enabled {
            Some(format!(
                "{:02}:{:02}",
                self.maintenance_hour.min(23),
                self.maintenance_minute.min(59)
            ))
        } else {
            None
        };
        cfg.schedule.verify_every_days = if self.verify_enabled {
            self.verify_every_days
        } else {
            0
        };
        cfg.schedule.notify_overdue_hours = if self.notify_enabled {
            self.notify_overdue_hours
        } else {
            0
        };
        cfg.retention.keep_last = self.keep_last;
        cfg.retention.keep_daily = self.keep_daily;
        cfg.retention.keep_weekly = self.keep_weekly;
        cfg.retention.keep_monthly = self.keep_monthly;
        cfg.agent.enabled = self.agent_enabled;
        cfg.agent.allow_restore = self.allow_restore;
        cfg.agent.allow_prune = self.allow_prune;
        cfg.agent.allow_delete_storage = self.allow_delete_storage;
        cfg.agent.allow_handoff = self.allow_handoff;
        cfg.start_minimized_to_tray = self.start_minimized;
    }
}

/// New credential values. A blank field keeps the stored value, so secrets are never
/// loaded back into the window just to be shown as dots.
#[derive(Default)]
struct SecretsDraft {
    access_key_id: String,
    secret_access_key: String,
    restic_password: String,
}

impl SecretsDraft {
    fn is_empty(&self) -> bool {
        self.access_key_id.trim().is_empty()
            && self.secret_access_key.trim().is_empty()
            && self.restic_password.is_empty()
    }

    fn wipe(&mut self) {
        self.access_key_id.zeroize();
        self.secret_access_key.zeroize();
        self.restic_password.zeroize();
    }
}

#[derive(Default)]
struct TargetDraft {
    label: String,
    excludes: String,
    tags: String,
    project_key: String,
}

struct SetupStep {
    done: bool,
    title: &'static str,
    detail: &'static str,
    page: AppTab,
}

pub struct ResticTrayApp {
    controller: Arc<SharedController>,
    /// Keeps the tray icon alive; its events are handled in `tray.rs`.
    _tray: Option<SystemTrayHandle>,
    tx: Sender<UiWorkerEvent>,
    rx: Receiver<UiWorkerEvent>,
    tokens: DesignTokens,

    page: AppTab,
    page_shown_at: f64,

    status: AppStatusReport,
    runs: Vec<BackupRunRecord>,
    audits: Vec<AuditLogRecord>,
    sync_report: Option<ProjectSyncReport>,
    sync_loading: bool,
    startup_sync_triggered: bool,
    last_poll: Instant,
    poll_in_flight: bool,
    /// Poll on the next frame instead of waiting for the interval (after starting or
    /// finishing a job). A flag, because `Instant::now() - d` panics right after boot.
    poll_soon: bool,
    jobs: Vec<Job>,

    target_draft: TargetDraft,
    editing_target: Option<String>,
    new_exclude: String,

    snapshots: Vec<ResticSnapshot>,
    snapshots_loaded: bool,
    snapshot_op: Option<SnapshotOp>,
    snapshot_filter_project: Option<String>,
    snapshot_filter_host: Option<String>,
    takeover_preview: Option<Result<TakeoverReport, String>>,
    takeover_preview_loading: bool,
    takeover_preview_seq: u64,
    forget_prune_preview: Option<String>,
    /// Folder sizes of the latest snapshot, once asked for on the Folders page.
    sizes: Option<Vec<FolderBytes>>,
    sizes_loading: bool,
    selected_snapshot: Option<String>,
    compare_snapshot: Option<String>,
    snapshot_files: Option<(String, Vec<ResticLsEntry>)>,
    diff_report: Option<ResticDiffReport>,
    restore_dir: String,
    restore_includes: String,

    history_view: HistoryView,

    settings: SettingsForm,
    /// The saved settings `settings` was last synced with, to notice changes made elsewhere.
    settings_base: SettingsForm,
    settings_dirty: bool,
    secrets_draft: SecretsDraft,
    reveal_secrets: bool,
    show_advanced_r2: bool,
    show_advanced_rclone: bool,
    show_danger_r2: bool,
    delete_confirm_input: String,
    delete_hold_started: Option<f64>,
    delete_r2_preview: Option<Result<R2DeleteReport, String>>,
    delete_r2_preview_loading: bool,
    last_r2_delete_note: Option<String>,

    toasts: Vec<Toast>,
    next_toast_id: u64,
    confirm: Option<Confirm>,

    /// Start in the tray (`--minimized` or the setting) until the window is hidden once.
    start_hidden: bool,
    /// The window is hidden in the tray; the frame loop must stay idle until it is shown.
    in_tray: bool,

    // Automated screenshot support (`--screenshot <path>`)
    screenshot_path: Option<PathBuf>,
    screenshot_requested: bool,
    frame_count: u32,
    started_instant: Instant,
}

const SIDEBAR_WIDTH: f32 = 232.0;
const CONTENT_MAX_WIDTH: f32 = 1040.0;

impl ResticTrayApp {
    pub fn new(
        cc: &eframe::CreationContext<'_>,
        controller: Arc<SharedController>,
        initial_tab: AppTab,
        initial_dark: Option<bool>,
        start_hidden: bool,
        screenshot_path: Option<PathBuf>,
    ) -> Self {
        theme::install_fonts(&cc.egui_ctx);

        let cfg = controller.get_config();
        let tokens = DesignTokens::for_theme(initial_dark.unwrap_or(cfg.theme != "light"));
        tokens.apply(&cc.egui_ctx);

        crate::window::register(cc);
        let _tray = SystemTrayHandle::try_create(&controller);
        let (tx, rx) = channel();
        let status = controller.status_report();
        let (runs, audits) = load_history(&controller);
        let sync_report = controller
            .get_project_sync_status("gui", None, false, false)
            .ok();

        Self {
            controller,
            _tray,
            tx,
            rx,
            tokens,
            page: initial_tab,
            page_shown_at: 0.0,
            status,
            runs,
            audits,
            sync_report,
            sync_loading: false,
            startup_sync_triggered: false,
            last_poll: Instant::now(),
            poll_in_flight: false,
            poll_soon: false,
            jobs: Vec::new(),
            target_draft: TargetDraft::default(),
            editing_target: None,
            new_exclude: String::new(),
            snapshots: Vec::new(),
            snapshots_loaded: false,
            snapshot_op: None,
            snapshot_filter_project: None,
            snapshot_filter_host: None,
            takeover_preview: None,
            takeover_preview_loading: false,
            takeover_preview_seq: 0,
            forget_prune_preview: None,
            sizes: None,
            sizes_loading: false,
            selected_snapshot: None,
            compare_snapshot: None,
            snapshot_files: None,
            diff_report: None,
            restore_dir: String::new(),
            restore_includes: String::new(),
            history_view: HistoryView::Runs,
            settings: SettingsForm::from_config(&cfg),
            settings_base: SettingsForm::from_config(&cfg),
            settings_dirty: false,
            secrets_draft: SecretsDraft::default(),
            reveal_secrets: false,
            show_advanced_r2: false,
            show_advanced_rclone: false,
            show_danger_r2: false,
            delete_confirm_input: String::new(),
            delete_hold_started: None,
            delete_r2_preview: None,
            delete_r2_preview_loading: false,
            last_r2_delete_note: None,
            toasts: Vec::new(),
            next_toast_id: 0,
            confirm: None,
            start_hidden,
            in_tray: false,
            screenshot_path,
            screenshot_requested: false,
            frame_count: 0,
            started_instant: Instant::now(),
        }
    }

    // -----------------------------------------------------------------------------------------
    // Feedback
    // -----------------------------------------------------------------------------------------

    fn toast(&mut self, kind: ToastKind, message: impl Into<String>) {
        let raw: String = message.into();
        // restic errors can carry a full stderr dump; the toast keeps the
        // actionable part and the history page keeps the whole text.
        let message = if kind == ToastKind::Error {
            format::summarize_error_for_ui(&raw, 600)
        } else if let Some((cut, _)) = raw.char_indices().nth(600) {
            let mut out = raw[..cut].to_owned();
            out.push('…');
            out
        } else {
            raw
        };
        self.next_toast_id += 1;
        self.toasts.push(Toast {
            id: self.next_toast_id,
            message,
            kind,
            born: f64::NAN,
        });
        if self.toasts.len() > 4 {
            self.toasts.remove(0);
        }
    }

    fn toast_ok(&mut self, message: impl Into<String>) {
        self.toast(ToastKind::Success, message);
    }

    fn toast_err(&mut self, message: impl Into<String>) {
        self.toast(ToastKind::Error, message);
    }

    fn go(&mut self, page: AppTab) {
        if self.page != page {
            self.page = page;
            self.page_shown_at = f64::NAN;
        }
    }

    // -----------------------------------------------------------------------------------------
    // Background work
    // -----------------------------------------------------------------------------------------

    fn refresh_telemetry_async(&mut self) {
        if self.poll_in_flight {
            return;
        }
        self.poll_in_flight = true;
        self.last_poll = Instant::now();
        let ctrl = Arc::clone(&self.controller);
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let report = Box::new(ctrl.status_report());
            let (runs, audits) = load_history(&ctrl);
            let sync_report = ctrl.get_project_sync_status("gui", None, false, false).ok();
            let _ = tx.send(UiWorkerEvent::StatusRefreshed {
                report,
                runs,
                audits,
                sync_report,
            });
        });
    }

    fn job_running(&self, job: Job) -> bool {
        self.jobs.contains(&job)
    }

    fn backup_active(&self) -> bool {
        self.status.active_progress.running
            || self.job_running(Job::Backup)
            || self.job_running(Job::DryRun)
            || self.job_running(Job::ReleaseProject)
            || self.job_running(Job::TakeoverProject)
    }

    fn is_multi_pc_shared(&self) -> bool {
        self.controller.get_config().has_shared_projects()
            || !self.status.observed_remote_hosts.is_empty()
    }

    fn refresh_sync_status_async(&mut self, check_local_changes: bool) {
        if self.sync_loading {
            return;
        }
        self.sync_loading = true;
        let ctrl = Arc::clone(&self.controller);
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let res = ctrl
                .refresh_project_sync_status("gui", check_local_changes)
                .map_err(|e| e.to_string());
            let _ = tx.send(UiWorkerEvent::SyncReportLoaded(res));
        });
    }

    fn open_takeover_confirm(&mut self, project_key: String, label: String, local_path: String) {
        self.takeover_preview = None;
        self.takeover_preview_loading = false;
        self.confirm = Some(Confirm::TakeoverProject {
            project_key: project_key.clone(),
            label,
            local_path: local_path.clone(),
            delete_extra: true,
            save_local_backup_first: true,
            discard_confirmed: false,
            force_while_remote_active: false,
        });
        self.takeover_preview_seq = self.takeover_preview_seq.wrapping_add(1);
        self.takeover_preview = None;
        self.takeover_preview_loading = false;
        if !local_path.trim().is_empty() {
            self.start_takeover_preview(project_key, local_path, true, true);
        }
    }

    fn start_takeover_preview(
        &mut self,
        project_key: String,
        local_path: String,
        delete_extra: bool,
        save_local_backup_first: bool,
    ) {
        if self.job_running(Job::TakeoverProject) {
            return;
        }
        self.takeover_preview_seq = self.takeover_preview_seq.wrapping_add(1);
        let req_seq = self.takeover_preview_seq;
        self.takeover_preview_loading = true;
        self.takeover_preview = None;
        let ctrl = Arc::clone(&self.controller);
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let res = ctrl
                .takeover_project(
                    "gui",
                    TakeoverParams {
                        project_key,
                        local_path: (!local_path.trim().is_empty()).then_some(local_path),
                        dry_run: true,
                        save_local_backup_first,
                        discard_local_changes: false,
                        force_while_remote_active: true,
                        delete_extra,
                        confirm_project_key: None,
                        expected_source_snapshot_id: None,
                        expected_canonical_path: None,
                    },
                )
                .map_err(|e| e.to_string());
            let _ = tx.send(UiWorkerEvent::TakeoverPreviewLoaded(req_seq, res));
        });
    }

    fn start_takeover(&mut self, params: TakeoverParams) {
        if self.job_running(Job::TakeoverProject) {
            return;
        }
        self.jobs.push(Job::TakeoverProject);
        let ctrl = Arc::clone(&self.controller);
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let res = ctrl
                .takeover_project("gui", params)
                .map_err(|e| e.to_string());
            let _ = tx.send(UiWorkerEvent::TakeoverFinished(res));
        });
        self.poll_soon = true;
    }

    fn start_release_project(&mut self, project_key: String, label: String) {
        self.spawn_job(Job::ReleaseProject, move |ctrl| {
            ctrl.release_project("gui", &project_key, false)
                .map(|_| format!("「{label}」を引き渡し済みにしました（待機中）"))
                .map_err(|e| e.to_string())
        });
        self.poll_soon = true;
    }

    fn start_unlock_repo(&mut self) {
        self.spawn_job(Job::UnlockRepo, move |ctrl| {
            ctrl.unlock_repository("gui")
                .map(|_| "古いロックの確認・解除が完了しました".to_owned())
                .map_err(|e| e.to_string())
        });
    }

    fn start_forget_prune(&mut self, dry_run: bool) {
        if dry_run {
            if self.job_running(Job::ForgetPrune) {
                return;
            }
            self.jobs.push(Job::ForgetPrune);
            self.forget_prune_preview = None;
            let ctrl = Arc::clone(&self.controller);
            let tx = self.tx.clone();
            std::thread::spawn(move || {
                let res = ctrl
                    .forget_prune("gui", true)
                    .map(|raw| format_forget_preview_for_ui(&raw))
                    .map_err(|e| e.to_string());
                let _ = tx.send(UiWorkerEvent::ForgetPrunePreviewLoaded(res));
            });
            self.poll_soon = true;
            return;
        }
        self.forget_prune_preview = None;
        self.spawn_job(Job::ForgetPrune, move |ctrl| {
            ctrl.forget_prune("gui", false)
                .map(|_| "古いスナップショットの整理が完了しました".to_owned())
                .map_err(|e| e.to_string())
        });
    }

    fn start_forget_snapshot(&mut self, snapshot_id: String) {
        self.spawn_job(Job::ForgetSnapshot, move |ctrl| {
            ctrl.forget_snapshot("gui", &snapshot_id)
                .map(|_| "退避スナップショットを削除しました".to_owned())
                .map_err(|e| e.to_string())
        });
    }

    fn spawn_job(
        &mut self,
        job: Job,
        work: impl FnOnce(&SharedController) -> Result<String, String> + Send + 'static,
    ) {
        if self.job_running(job) {
            return;
        }
        self.jobs.push(job);
        let ctrl = Arc::clone(&self.controller);
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let result = work(&ctrl);
            let _ = tx.send(UiWorkerEvent::JobFinished(job, result));
        });
    }

    fn start_backup(&mut self, dry_run: bool) {
        let job = if dry_run { Job::DryRun } else { Job::Backup };
        self.spawn_job(job, move |ctrl| {
            ctrl.execute_backup("gui", None, dry_run, &[])
                .map(|result| {
                    let unreadable = result["summary"]["unreadable"]
                        .as_array()
                        .map_or(0, Vec::len);
                    match (dry_run, unreadable) {
                        (true, 0) => "テスト実行が完了しました。結果は履歴で確認できます".to_owned(),
                        (true, n) => format!(
                            "テスト実行が完了しました。読み取れなかった項目が {n} 件あります。履歴で確認できます"
                        ),
                        (false, 0) => "バックアップが完了しました".to_owned(),
                        (false, n) => format!(
                            "バックアップは完了しましたが、読み取れなかった項目が {n} 件あります。履歴で確認できます"
                        ),
                    }
                })
                .map_err(|e| e.to_string())
        });
        // The progress state flips to running within a second; poll sooner than usual.
        self.poll_soon = true;
    }

    fn start_repo_job(&mut self, job: Job) {
        self.spawn_job(job, move |ctrl| {
            match job {
                Job::InitRepo => ctrl
                    .init_repository("gui")
                    .map(|_| "R2 にリポジトリを作成しました".to_owned()),
                _ => ctrl
                    .check_repository("gui")
                    .map(|_| "接続と整合性を確認しました。問題はありません".to_owned()),
            }
            .map_err(|e| e.to_string())
        });
    }

    fn start_restore(&mut self, snapshot_id: String, target_dir: String, includes: Vec<String>) {
        self.spawn_job(Job::Restore, move |ctrl| {
            ctrl.restore_snapshot("gui", &snapshot_id, Path::new(&target_dir), &includes)
                .map(|_| format!("{target_dir} に復元しました"))
                .map_err(|e| e.to_string())
        });
    }

    fn open_delete_r2_confirm(&mut self, scope: R2DeleteScope) {
        let cfg = self.controller.get_config();
        let bucket = cfg.r2.normalized_bucket();
        let prefix = cfg.r2.normalized_prefix();
        let endpoint = match cfg.r2.endpoint_url() {
            Ok(url) => url,
            Err(err) => {
                self.toast_err(err.to_string());
                return;
            }
        };
        if scope == R2DeleteScope::PrefixOnly && prefix.is_empty() {
            self.toast_err(
                "プレフィックスが空のため「プレフィックス配下のみ削除」は実行できません",
            );
            return;
        }
        self.delete_confirm_input.clear();
        self.delete_hold_started = None;
        self.delete_r2_preview = None;
        self.delete_r2_preview_loading = false;
        self.confirm = Some(Confirm::DeleteR2 {
            scope,
            bucket,
            prefix,
            endpoint,
        });
    }

    fn start_r2_delete_preview(&mut self, scope: R2DeleteScope) {
        if self.delete_r2_preview_loading || self.job_running(Job::DeleteR2) {
            return;
        }
        self.delete_r2_preview_loading = true;
        self.delete_r2_preview = None;
        let ctrl = Arc::clone(&self.controller);
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let res = ctrl
                .delete_r2_data("gui", scope, None, true)
                .map_err(|e| e.to_string());
            let _ = tx.send(UiWorkerEvent::R2DeletePreviewLoaded(res));
        });
        self.poll_soon = true;
    }

    fn start_r2_delete(&mut self, scope: R2DeleteScope, bucket: String) {
        if self.job_running(Job::DeleteR2) {
            return;
        }
        self.jobs.push(Job::DeleteR2);
        self.last_r2_delete_note = None;
        let ctrl = Arc::clone(&self.controller);
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let res = ctrl
                .delete_r2_data("gui", scope, Some(&bucket), false)
                .map_err(|e| e.to_string());
            let _ = tx.send(UiWorkerEvent::R2DeleteFinished(res));
        });
        self.poll_soon = true;
    }

    fn fetch_snapshots(&mut self) {
        if self.snapshot_op.is_some() {
            return;
        }
        self.snapshot_op = Some(SnapshotOp::List);
        let ctrl = Arc::clone(&self.controller);
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let cfg = ctrl.get_config();
            let res = ctrl
                .load_secrets()
                .and_then(|sec| runner::list_snapshots(&cfg, &sec, None, None))
                .map_err(|e| e.to_string());
            let _ = tx.send(UiWorkerEvent::SnapshotsLoaded(res));
        });
    }

    fn fetch_snapshot_files(&mut self, snap_id: String) {
        if self.snapshot_op.is_some() {
            return;
        }
        self.snapshot_op = Some(SnapshotOp::Files);
        let ctrl = Arc::clone(&self.controller);
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let cfg = ctrl.get_config();
            let res = ctrl
                .load_secrets()
                .and_then(|sec| runner::list_snapshot_files(&cfg, &sec, &snap_id, None, 250))
                .map(|entries| (snap_id, entries))
                .map_err(|e| e.to_string());
            let _ = tx.send(UiWorkerEvent::SnapshotFilesLoaded(res));
        });
    }

    fn fetch_diff(&mut self, older: String, newer: String) {
        if self.snapshot_op.is_some() {
            return;
        }
        self.snapshot_op = Some(SnapshotOp::Diff);
        let ctrl = Arc::clone(&self.controller);
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let cfg = ctrl.get_config();
            let res = ctrl
                .load_secrets()
                .and_then(|sec| runner::diff_snapshots(&cfg, &sec, &older, &newer))
                .map_err(|e| e.to_string());
            let _ = tx.send(UiWorkerEvent::DiffLoaded(res));
        });
    }

    fn fetch_sizes(&mut self) {
        if self.sizes_loading {
            return;
        }
        self.sizes_loading = true;
        let ctrl = Arc::clone(&self.controller);
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let res = ctrl
                .snapshot_folder_sizes("latest")
                .map_err(|e| e.to_string());
            let _ = tx.send(UiWorkerEvent::SizesLoaded(res));
        });
    }

    fn drain_events(&mut self) {
        while let Ok(ev) = self.rx.try_recv() {
            match ev {
                UiWorkerEvent::StatusRefreshed {
                    report,
                    runs,
                    audits,
                    sync_report,
                } => {
                    self.poll_in_flight = false;
                    self.status = *report;
                    self.runs = runs;
                    self.audits = audits;
                    if sync_report.is_some() {
                        self.sync_report = sync_report;
                    }
                }
                UiWorkerEvent::SyncReportLoaded(res) => {
                    self.sync_loading = false;
                    match res {
                        Ok(report) => {
                            self.sync_report = Some(report);
                            self.poll_soon = true;
                        }
                        Err(err) => self.toast_err(err),
                    }
                }
                UiWorkerEvent::TakeoverPreviewLoaded(seq, res) => {
                    let matches_open = matches!(
                        &self.confirm,
                        Some(Confirm::TakeoverProject { project_key, .. })
                            if !res
                                .as_ref()
                                .is_ok_and(|r| r.project_key != *project_key)
                    );
                    if seq == self.takeover_preview_seq && matches_open {
                        self.takeover_preview_loading = false;
                        self.takeover_preview = Some(res);
                    }
                }
                UiWorkerEvent::ForgetPrunePreviewLoaded(res) => {
                    self.jobs.retain(|j| *j != Job::ForgetPrune);
                    match res {
                        Ok(summary_text) => {
                            self.forget_prune_preview = Some(summary_text);
                            self.toast_ok("整理対象の確認 (Dry-Run) が完了しました");
                            self.poll_soon = true;
                        }
                        Err(err) => {
                            self.toast_err(err);
                            self.poll_soon = true;
                        }
                    }
                }
                UiWorkerEvent::TakeoverFinished(res) => {
                    self.jobs.retain(|j| *j != Job::TakeoverProject);
                    match res {
                        Ok(report) => {
                            self.toast_ok(format!(
                                "「{}」をこの PC で引き継ぎました（{}）",
                                report.project_key, report.local_path
                            ));
                            self.reset_snapshot_cache();
                            self.poll_soon = true;
                        }
                        Err(err) => {
                            self.toast_err(err);
                            self.poll_soon = true;
                        }
                    }
                }
                UiWorkerEvent::SnapshotsLoaded(res) => {
                    self.snapshot_op = None;
                    match res {
                        Ok(mut list) => {
                            list.sort_by(|a, b| b.time.cmp(&a.time));
                            if self.selected_snapshot.is_none() {
                                self.selected_snapshot = list.first().map(|s| s.id.clone());
                            }
                            self.snapshots = list;
                            self.snapshots_loaded = true;
                        }
                        Err(err) => self.toast_err(err),
                    }
                }
                UiWorkerEvent::SnapshotFilesLoaded(res) => {
                    self.snapshot_op = None;
                    match res {
                        Ok(files) => self.snapshot_files = Some(files),
                        Err(err) => self.toast_err(err),
                    }
                }
                UiWorkerEvent::SizesLoaded(res) => {
                    self.sizes_loading = false;
                    match res {
                        Ok(folders) => self.sizes = Some(folders),
                        Err(err) => self.toast_err(err),
                    }
                }
                UiWorkerEvent::DiffLoaded(res) => {
                    self.snapshot_op = None;
                    match res {
                        Ok(diff) => self.diff_report = Some(diff),
                        Err(err) => self.toast_err(err),
                    }
                }
                UiWorkerEvent::R2DeletePreviewLoaded(res) => {
                    self.delete_r2_preview_loading = false;
                    self.delete_r2_preview = Some(res);
                    self.poll_soon = true;
                }
                UiWorkerEvent::R2DeleteFinished(res) => {
                    self.jobs.retain(|j| *j != Job::DeleteR2);
                    match res {
                        Ok(report) => {
                            self.reset_snapshot_cache();
                            if report.bucket_delete_access_denied {
                                self.last_r2_delete_note = report.note.clone();
                                self.toast_ok(format!(
                                    "バケット「{}」を空にしました（{} 件 / {}）。本体は Cloudflare ダッシュボードから削除できます",
                                    report.bucket,
                                    format::count(report.objects_deleted),
                                    format::bytes(report.bytes_deleted),
                                ));
                            } else if report.bucket_not_found {
                                self.last_r2_delete_note = report.note.clone();
                                self.toast_ok(format!(
                                    "バケット「{}」は見つかりませんでした（すでに削除済みの可能性があります）",
                                    report.bucket
                                ));
                            } else if report.bucket_deleted {
                                self.last_r2_delete_note = None;
                                self.toast_ok(format!(
                                    "R2 バケット「{}」を削除しました（{} 件 / {}）",
                                    report.bucket,
                                    format::count(report.objects_deleted),
                                    format::bytes(report.bytes_deleted),
                                ));
                            } else {
                                self.last_r2_delete_note = None;
                                let prefix = report.prefix.as_deref().unwrap_or("");
                                self.toast_ok(format!(
                                    "R2 プレフィックス {}/{prefix} を削除しました（{} 件 / {}）",
                                    report.bucket,
                                    format::count(report.objects_deleted),
                                    format::bytes(report.bytes_deleted),
                                ));
                            }
                        }
                        Err(err) => self.toast_err(err),
                    }
                    self.poll_soon = true;
                }
                UiWorkerEvent::JobFinished(job, res) => {
                    self.jobs.retain(|j| *j != job);
                    if matches!(
                        job,
                        Job::ForgetSnapshot | Job::ForgetPrune | Job::ReleaseProject
                    ) && res.is_ok()
                    {
                        self.reset_snapshot_cache();
                    }
                    match res {
                        Ok(msg) => self.toast_ok(msg),
                        Err(err) => self.toast_err(err),
                    }
                    self.poll_soon = true;
                }
            }
        }
    }

    // -----------------------------------------------------------------------------------------
    // Shared state helpers
    // -----------------------------------------------------------------------------------------

    fn setup_steps(&self) -> Vec<SetupStep> {
        let rep = &self.status;
        vec![
            SetupStep {
                done: rep.r2_configured,
                title: "保存先の R2 バケットを指定する",
                detail: "Cloudflare のアカウント ID とバケット名を入力します",
                page: AppTab::Settings,
            },
            SetupStep {
                done: rep.secrets_readiness.is_complete(),
                title: "認証情報を保存する",
                detail: "R2 のアクセスキーと、バックアップを暗号化するパスワード",
                page: AppTab::Settings,
            },
            SetupStep {
                done: rep.restic_binary.available,
                title: "restic を用意する",
                detail: "scripts/download-restic.ps1 で公式版を取得できます",
                page: AppTab::Settings,
            },
            SetupStep {
                done: rep.total_target_count > 0 || rep.project_sync_summary.not_local > 0,
                title: "バックアップするフォルダを追加する",
                detail: "ウィンドウにフォルダをドロップしても追加できます",
                page: AppTab::Targets,
            },
        ]
    }

    fn set_theme(&mut self, dark: bool, ctx: &Context) {
        self.tokens = DesignTokens::for_theme(dark);
        self.tokens.apply(ctx);
        let theme = if dark { "dark" } else { "light" };
        if let Err(err) = self.controller.update_config(|cfg| {
            cfg.theme = theme.to_owned();
            Ok(())
        }) {
            self.toast_err(err.to_string());
        }
    }

    /// Adds or updates a folder target. Returns the label shown to the user.
    fn upsert_target(
        &self,
        path: &Path,
        label: Option<String>,
        excludes: Option<Vec<String>>,
        tags: Option<Vec<String>>,
        enabled: Option<bool>,
    ) -> Result<String, String> {
        self.upsert_target_full(path, label, excludes, tags, enabled, None, None)
    }

    #[allow(clippy::too_many_arguments)]
    fn upsert_target_full(
        &self,
        path: &Path,
        label: Option<String>,
        excludes: Option<Vec<String>>,
        tags: Option<Vec<String>>,
        enabled: Option<bool>,
        project_key: Option<Option<String>>,
        writer_state: Option<WriterState>,
    ) -> Result<String, String> {
        let mut saved_label = String::new();
        self.controller
            .update_config(|cfg| {
                saved_label = cfg
                    .upsert_target_full(
                        path,
                        label,
                        excludes,
                        tags,
                        enabled,
                        project_key,
                        writer_state,
                    )?
                    .label;
                Ok(())
            })
            .map(|_| saved_label)
            .map_err(|e| e.to_string())
    }

    fn handle_dropped_folders(&mut self, ctx: &Context) {
        let dropped: Vec<PathBuf> = ctx.input(|i| {
            i.raw
                .dropped_files
                .iter()
                .map(|f| f.path().to_path_buf())
                .collect()
        });
        if dropped.is_empty() {
            return;
        }
        let mut added = Vec::new();
        for path in dropped {
            if !path.is_dir() {
                let name = path.file_name().map_or_else(
                    || path.to_string_lossy().into_owned(),
                    |n| n.to_string_lossy().into_owned(),
                );
                self.toast_err(format!("{name} はフォルダではないため追加できません"));
                continue;
            }
            match self.upsert_target(&path, None, None, None, Some(true)) {
                Ok(label) => added.push(label),
                Err(err) => self.toast_err(err),
            }
        }
        match added.as_slice() {
            [] => {}
            [one] => self.toast_ok(format!("「{one}」をバックアップ対象に追加しました")),
            many => self.toast_ok(format!("{} 個のフォルダを追加しました", many.len())),
        }
        if !added.is_empty() {
            self.go(AppTab::Targets);
            self.refresh_telemetry_async();
        }
    }

    fn handle_shortcuts(&mut self, ctx: &Context) {
        let pages = [
            (Key::Num1, AppTab::Dashboard),
            (Key::Num2, AppTab::Targets),
            (Key::Num3, AppTab::Snapshots),
            (Key::Num4, AppTab::History),
            (Key::Num5, AppTab::Settings),
        ];
        for (key, page) in pages {
            if ctx.input_mut(|i| i.consume_key(Modifiers::COMMAND, key)) {
                self.go(page);
            }
        }
    }

    // -----------------------------------------------------------------------------------------
    // Chrome: sidebar, overlays
    // -----------------------------------------------------------------------------------------

    fn render_sidebar(&mut self, ui: &mut egui::Ui) {
        let t = self.tokens;
        egui::Panel::left("sidebar")
            .exact_size(SIDEBAR_WIDTH)
            .resizable(false)
            .frame(Frame::new().fill(t.sidebar).inner_margin(Margin {
                left: 14,
                right: 14,
                top: 22,
                bottom: 16,
            }))
            .show(ui, |ui| {
                egui::Panel::bottom("sidebar-footer")
                    .frame(Frame::new())
                    .show_separator_line(false)
                    .show(ui, |ui| self.sidebar_footer(ui));
                ui.horizontal(|ui| {
                    ui.add_space(6.0);
                    let (rect, _) = ui.allocate_exact_size(Vec2::splat(30.0), Sense::hover());
                    paint_brand_mark(ui.painter(), rect, t.accent, t.on_primary_mark());
                    ui.add_space(4.0);
                    ui.vertical(|ui| {
                        ui.spacing_mut().item_spacing.y = 0.0;
                        ui.label(
                            RichText::new("Restic Tray")
                                .font(strong(15.0))
                                .color(t.text),
                        );
                        ui.label(
                            RichText::new(format!("v{}", self.status.version))
                                .font(body(12.0))
                                .color(t.text_3),
                        );
                    });
                });
                ui.add_space(28.0);

                let setup_left = self.setup_steps().iter().filter(|s| !s.done).count();
                let target_count = self.status.total_target_count.to_string();
                let snapshot_count = self.snapshots.len().to_string();
                let items = [
                    (
                        AppTab::Dashboard,
                        VectorIcon::Overview,
                        "概要",
                        NavTrailing::None,
                    ),
                    (
                        AppTab::Targets,
                        VectorIcon::Folder,
                        "フォルダ",
                        NavTrailing::Text(&target_count),
                    ),
                    (
                        AppTab::Snapshots,
                        VectorIcon::Snapshots,
                        "スナップショット",
                        if self.snapshots_loaded {
                            NavTrailing::Text(&snapshot_count)
                        } else {
                            NavTrailing::None
                        },
                    ),
                    (
                        AppTab::History,
                        VectorIcon::History,
                        "履歴",
                        NavTrailing::None,
                    ),
                    (
                        AppTab::Settings,
                        VectorIcon::Settings,
                        "設定",
                        if setup_left > 0 {
                            NavTrailing::Dot(t.warn)
                        } else {
                            NavTrailing::None
                        },
                    ),
                ];
                let mut target_page = None;
                for (i, (page, icon, label, trailing)) in items.into_iter().enumerate() {
                    let r = nav_item(ui, &t, icon, label, self.page == page, trailing)
                        .on_hover_text(format!("Ctrl+{}", i + 1));
                    if r.clicked() {
                        target_page = Some(page);
                    }
                    ui.add_space(2.0);
                }
                if let Some(page) = target_page {
                    self.go(page);
                }
            });
    }

    fn sidebar_footer(&mut self, ui: &mut egui::Ui) {
        let t = self.tokens;
        self.sidebar_status(ui);
        ui.add_space(10.0);
        ui.horizontal(|ui| {
            let dark = t.is_dark;
            let (icon, label) = if dark {
                (VectorIcon::Sun, "ライトテーマに切り替え")
            } else {
                (VectorIcon::Moon, "ダークテーマに切り替え")
            };
            if icon_button(ui, &t, icon, label).clicked() {
                self.set_theme(!dark, ui.ctx());
            }
            if icon_button(ui, &t, VectorIcon::HideToTray, "トレイに格納").clicked() {
                ui.ctx()
                    .send_viewport_cmd(egui::ViewportCommand::Visible(false));
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if icon_button(ui, &t, VectorIcon::Power, "Restic Tray を終了").clicked() {
                    self.confirm = Some(Confirm::Quit);
                }
            });
        });
    }

    fn sidebar_status(&mut self, ui: &mut egui::Ui) {
        let t = self.tokens;
        let now = chrono::Local::now();
        theme::panel_frame(&t)
            .fill(t.sidebar)
            .inner_margin(Margin::same(14))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.spacing_mut().item_spacing.y = 4.0;
                let p = &self.status.active_progress;
                if self.backup_active() {
                    ui.horizontal(|ui| {
                        let (r, _) = ui.allocate_exact_size(Vec2::splat(12.0), Sense::hover());
                        paint_status_dot(ui, r.center(), t.ok(), true);
                        ui.label(
                            RichText::new("バックアップ中")
                                .font(strong(13.0))
                                .color(t.text),
                        );
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            if p.running {
                                ui.label(
                                    RichText::new(format!("{:.0}%", p.percent_done * 100.0))
                                        .font(theme::display(15.0))
                                        .color(t.text),
                                );
                            }
                        });
                    });
                    ui.add_space(4.0);
                    let fraction =
                        (p.running && p.percent_done > 0.0).then_some(p.percent_done as f32);
                    progress_bar(ui, &t, fraction, 4.0);
                    return;
                }
                let (dot, head) = if self.status.schedule_enabled {
                    (t.ok(), "自動バックアップ")
                } else {
                    (t.text_3, "自動バックアップは停止中")
                };
                ui.horizontal(|ui| {
                    let (r, _) = ui.allocate_exact_size(Vec2::splat(12.0), Sense::hover());
                    paint_status_dot(ui, r.center(), dot, false);
                    ui.label(RichText::new(head).font(body(12.5)).color(t.text_2));
                });
                let next = self
                    .status
                    .next_scheduled_at
                    .as_deref()
                    .and_then(format::parse_local);
                if let Some(next) = next.filter(|_| self.status.schedule_enabled) {
                    ui.horizontal(|ui| {
                        ui.label(
                            RichText::new(format::clock(next))
                                .font(theme::display(24.0))
                                .color(t.text),
                        );
                        ui.label(
                            RichText::new(format::until(next, now))
                                .font(body(12.5))
                                .color(t.text_2),
                        );
                    });
                    ui.label(
                        RichText::new(format::schedule(
                            &self.status.daily_at,
                            self.status.interval_minutes,
                        ))
                        .font(body(12.0))
                        .color(t.text_3),
                    );
                }
            });
    }

    fn render_drop_overlay(&self, ctx: &Context) {
        if ctx.input(|i| i.raw.hovered_files.is_empty()) {
            return;
        }
        let t = self.tokens;
        let screen = ctx.content_rect();
        let painter =
            ctx.layer_painter(LayerId::new(Order::Foreground, Id::unique("drop-overlay")));
        painter.rect_filled(screen, CornerRadius::ZERO, t.bg.gamma_multiply(0.92));
        let frame = screen.shrink(20.0);
        let corners = [
            frame.left_top(),
            frame.right_top(),
            frame.right_bottom(),
            frame.left_bottom(),
            frame.left_top(),
        ];
        painter.extend(Shape::dashed_line(
            &corners,
            Stroke::new(2.0_f32, t.accent_text),
            10.0,
            8.0,
        ));
        let c = screen.center();
        paint_icon(
            &painter,
            Rect::from_center_size(c - Vec2::new(0.0, 44.0), Vec2::splat(44.0)),
            VectorIcon::Download,
            t.accent_text,
        );
        painter.text(
            c + Vec2::new(0.0, 12.0),
            Align2::CENTER_CENTER,
            "ドロップしてバックアップ対象に追加",
            strong(20.0),
            t.text,
        );
        painter.text(
            c + Vec2::new(0.0, 42.0),
            Align2::CENTER_CENTER,
            "フォルダだけを追加します。ファイルは無視されます",
            body(13.5),
            t.text_2,
        );
    }

    fn render_toasts(&mut self, ctx: &Context) {
        let now = ctx.input(|i| i.time);
        for toast in &mut self.toasts {
            if toast.born.is_nan() {
                toast.born = now;
            }
        }
        self.toasts.retain(|toast| {
            let ttl = match toast.kind {
                ToastKind::Success => 4.5,
                ToastKind::Error => 14.0,
            };
            now - toast.born < ttl
        });
        if self.toasts.is_empty() {
            return;
        }
        ctx.request_repaint_after(Duration::from_millis(250));

        let t = self.tokens;
        let bottom = if self.page == AppTab::Settings && self.settings_dirty {
            96.0
        } else {
            24.0
        };
        let mut dismiss = None;
        egui::Area::new(Id::unique("toasts"))
            .anchor(Align2::RIGHT_BOTTOM, Vec2::new(-24.0, -bottom))
            .order(Order::Foreground)
            .show(ctx, |ui| {
                ui.set_max_width(400.0);
                for toast in &self.toasts {
                    let age = (now - toast.born) as f32;
                    let k = (age / 0.2).clamp(0.0, 1.0);
                    if k < 1.0 {
                        ui.ctx().request_repaint();
                    }
                    let eased = 1.0 - (1.0 - k).powi(3);
                    ui.scope(|ui| {
                        ui.multiply_opacity(eased);
                        let (icon, color) = match toast.kind {
                            ToastKind::Success => (VectorIcon::Check, t.ok()),
                            ToastKind::Error => (VectorIcon::Alert, t.err),
                        };
                        Frame::new()
                            .fill(t.surface)
                            .stroke(Stroke::new(1.0_f32, t.line_strong))
                            .corner_radius(CornerRadius::same(12))
                            .inner_margin(Margin {
                                left: 14,
                                right: 6,
                                top: 6,
                                bottom: 6,
                            })
                            .shadow(ctx.global_style().visuals.popup_shadow)
                            .show(ui, |ui| {
                                ui.set_width(372.0);
                                let buttons = if toast.kind == ToastKind::Error {
                                    2.0
                                } else {
                                    1.0
                                };
                                ui.horizontal_top(|ui| {
                                    ui.spacing_mut().item_spacing.x = 4.0;
                                    let (r, _) = ui
                                        .allocate_exact_size(Vec2::new(18.0, 32.0), Sense::hover());
                                    paint_icon(
                                        ui.painter(),
                                        Rect::from_center_size(r.center(), Vec2::splat(18.0)),
                                        icon,
                                        color,
                                    );
                                    ui.add_space(6.0);
                                    ui.vertical(|ui| {
                                        ui.set_width(372.0 - 32.0 - buttons * 36.0);
                                        ui.add_space(7.0);
                                        ui.add(
                                            egui::Label::new(
                                                RichText::new(&toast.message)
                                                    .font(body(13.5))
                                                    .color(t.text),
                                            )
                                            .wrap(),
                                        );
                                    });
                                    ui.with_layout(Layout::right_to_left(Align::Min), |ui| {
                                        if icon_button(ui, &t, VectorIcon::Close, "閉じる")
                                            .clicked()
                                        {
                                            dismiss = Some(toast.id);
                                        }
                                        if toast.kind == ToastKind::Error
                                            && icon_button(
                                                ui,
                                                &t,
                                                VectorIcon::Copy,
                                                "エラー内容をコピー",
                                            )
                                            .clicked()
                                        {
                                            ui.ctx().copy_text(toast.message.clone());
                                        }
                                    });
                                });
                            });
                    });
                    ui.add_space(8.0);
                }
            });
        if let Some(id) = dismiss {
            self.toasts.retain(|toast| toast.id != id);
        }
    }

    fn render_confirm(&mut self, ctx: &Context) {
        match &self.confirm {
            Some(Confirm::DeleteR2 { .. }) => {
                self.render_delete_r2_confirm(ctx);
                return;
            }
            Some(Confirm::TakeoverProject { .. }) => {
                self.render_takeover_confirm(ctx);
                return;
            }
            _ => {}
        }
        let Some(confirm) = &self.confirm else {
            return;
        };
        let t = self.tokens;
        let (title, text, detail, action, danger) = match confirm {
            Confirm::RemoveTarget { label, .. } => (
                "フォルダを対象から外しますか？",
                format!(
                    "「{label}」は今後バックアップされません。R2 に保存済みのスナップショットは削除されません。"
                ),
                None,
                "対象から外す",
                true,
            ),
            Confirm::Restore {
                snapshot_label,
                target_dir,
                includes,
                ..
            } => (
                "このスナップショットを復元しますか？",
                format!(
                    "{snapshot_label} の内容を次のフォルダに書き出します。同じ名前のファイルは上書きされます。"
                ),
                Some(if includes.is_empty() {
                    target_dir.clone()
                } else {
                    format!("{target_dir}\n対象: {}", includes.join(", "))
                }),
                "復元する",
                true,
            ),
            Confirm::InitRepo => (
                "R2 にリポジトリを作成しますか？",
                "初回のみ必要な操作です。保存したパスワードでリポジトリを暗号化します。パスワードを紛失すると復元できなくなるため、別の場所にも控えてください。".to_owned(),
                None,
                "作成する",
                false,
            ),
            Confirm::ReleaseProject {
                project_key,
                label,
                path,
            } => (
                "作業を終了して他 PC へ引き渡しますか？",
                format!(
                    "「{label}」を handoff:released タグ付きでバックアップし、この PC の作業権を待機中（standby）に切り替えます。ローカルファイルは削除されません。Unity エディタは閉じてから実行してください。"
                ),
                Some(format!("project:{project_key}\n{path}")),
                "引き渡しを実行",
                false,
            ),
            Confirm::ForgetPrune {
                dry_run,
                non_primary_warning,
            } => {
                if *dry_run {
                    (
                        "古い世代の整理対象を確認しますか？",
                        "Dry-Run で保持ポリシーに従う削除候補を確認します。スナップショットは削除されません。".to_owned(),
                        None,
                        "確認を実行",
                        false,
                    )
                } else if *non_primary_warning {
                    (
                        "メンテナンス担当外の PC から整理を実行しますか？",
                        "この PC は maintenanceRole: none（メンテナンス担当外）に設定されています。排他ロックを取得して forget --prune を実行すると、他 PC のバックアップや引き継ぎが一時的に待機状態になります。".to_owned(),
                        Some("通常は maintenanceRole: primary の PC で実行することを推奨します。".to_owned()),
                        "整理を実行",
                        true,
                    )
                } else {
                    (
                        "古いスナップショットを整理しますか？",
                        "保持ルールに従って古いスナップショットを削除し prune します。退避スナップショット（handoff:safety-backup）は保護されます。".to_owned(),
                        None,
                        "整理を実行",
                        true,
                    )
                }
            }
            Confirm::ForgetSnapshot { snapshot_label, .. } => (
                "退避スナップショットを削除しますか？",
                format!(
                    "{snapshot_label} をリポジトリから削除します。この操作は取り消せません。"
                ),
                None,
                "削除する",
                true,
            ),
            Confirm::UnlockRepo => (
                "古いロックを解除しますか？",
                "30 分以上更新されていない失効ロックのみを安全に解除します（restic unlock）。現在アクティブな他 PC のロックは削除されません。".to_owned(),
                None,
                "ロックを解除",
                false,
            ),
            Confirm::Quit => (
                "Restic Tray を終了しますか？",
                "終了すると、自動バックアップと MCP サーバーも停止します。ウィンドウだけを閉じる場合は「トレイに格納」を使ってください。".to_owned(),
                None,
                "終了する",
                true,
            ),
            Confirm::DeleteR2 { .. } | Confirm::TakeoverProject { .. } => {
                unreachable!("handled by dedicated confirm dialogs")
            }
        };

        let mut decision = None;
        let modal = egui::Modal::new(Id::unique("confirm"))
            .backdrop_color(Color32::from_black_alpha(if t.is_dark { 170 } else { 90 }))
            .frame(
                Frame::new()
                    .fill(t.surface)
                    .stroke(Stroke::new(1.0_f32, t.line_strong))
                    .corner_radius(CornerRadius::same(16))
                    .inner_margin(Margin::same(26))
                    .shadow(ctx.global_style().visuals.window_shadow),
            )
            .show(ctx, |ui| {
                ui.set_width(440.0);
                ui.label(RichText::new(title).font(strong(19.0)).color(t.text));
                ui.add_space(6.0);
                ui.label(RichText::new(text).font(body(14.0)).color(t.text_2));
                if let Some(detail) = &detail {
                    ui.add_space(10.0);
                    Frame::new()
                        .fill(t.bg)
                        .corner_radius(CornerRadius::same(8))
                        .inner_margin(Margin::symmetric(12, 10))
                        .show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            ui.label(RichText::new(detail).font(mono(12.5)).color(t.text));
                        });
                }
                ui.add_space(22.0);
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    let action_btn = if danger {
                        Btn::danger(action)
                    } else {
                        Btn::primary(action)
                    };
                    if action_btn.show(ui, &t).clicked() {
                        decision = Some(true);
                    }
                    if Btn::ghost("キャンセル").show(ui, &t).clicked() {
                        decision = Some(false);
                    }
                });
            });
        if decision.is_none() && modal.should_close() {
            decision = Some(false);
        }
        match decision {
            Some(true) => {
                if let Some(confirm) = self.confirm.take() {
                    self.run_confirmed(confirm, ctx);
                }
            }
            Some(false) => self.confirm = None,
            None => {}
        }
    }

    fn run_confirmed(&mut self, confirm: Confirm, ctx: &Context) {
        match confirm {
            Confirm::RemoveTarget { id, label } => {
                match self.controller.update_config(|c| {
                    c.remove_target(&id);
                    Ok(())
                }) {
                    Ok(_) => self.toast_ok(format!("「{label}」を対象から外しました")),
                    Err(err) => self.toast_err(err.to_string()),
                }
                if self.editing_target.as_deref() == Some(id.as_str()) {
                    self.editing_target = None;
                }
                self.refresh_telemetry_async();
            }
            Confirm::Restore {
                snapshot_id,
                target_dir,
                includes,
                ..
            } => self.start_restore(snapshot_id, target_dir, includes),
            Confirm::InitRepo => self.start_repo_job(Job::InitRepo),
            Confirm::DeleteR2 { scope, bucket, .. } => self.start_r2_delete(scope, bucket),
            Confirm::ReleaseProject {
                project_key, label, ..
            } => self.start_release_project(project_key, label),
            Confirm::ForgetPrune { dry_run, .. } => self.start_forget_prune(dry_run),
            Confirm::ForgetSnapshot { snapshot_id, .. } => self.start_forget_snapshot(snapshot_id),
            Confirm::UnlockRepo => self.start_unlock_repo(),
            Confirm::TakeoverProject { .. } => {}
            Confirm::Quit => {
                self.controller.request_quit();
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
        }
    }

    fn render_delete_r2_confirm(&mut self, ctx: &Context) {
        let Some(Confirm::DeleteR2 {
            scope,
            bucket,
            prefix,
            endpoint,
        }) = self.confirm.clone()
        else {
            return;
        };
        let t = self.tokens;
        let (title, desc, target_scope_line) = match scope {
            R2DeleteScope::PrefixOnly => (
                "R2 プレフィックス配下のデータを削除しますか？",
                "指定したプレフィックス配下のオブジェクトと未完了のマルチパートアップロードをすべて削除します。バケット本体や他のプレフィックスは削除されません。この操作は取り消せません。",
                format!("対象範囲: /{prefix}/ 以下のみ"),
            ),
            R2DeleteScope::EntireBucket => (
                "R2 バケットを完全に削除しますか？",
                "バケット内のすべてのオブジェクトと未完了のマルチパートアップロードを削除したうえで、バケット本体の削除を試行します。この操作は取り消せません。",
                "対象範囲: バケット全体（すべてのオブジェクト＋バケット本体）".to_owned(),
            ),
        };
        let detail = format!("バケット: {bucket}\n{target_scope_line}\nエンドポイント: {endpoint}");

        let mut decision = None;
        let mut run_preview = false;
        let modal = egui::Modal::new(Id::unique("confirm-delete-r2"))
            .backdrop_color(Color32::from_black_alpha(if t.is_dark { 170 } else { 90 }))
            .frame(
                Frame::new()
                    .fill(t.surface)
                    .stroke(Stroke::new(1.0_f32, t.line_strong))
                    .corner_radius(CornerRadius::same(16))
                    .inner_margin(Margin::same(26))
                    .shadow(ctx.global_style().visuals.window_shadow),
            )
            .show(ctx, |ui| {
                ui.set_width(480.0);
                ui.label(RichText::new(title).font(strong(19.0)).color(t.text));
                ui.add_space(6.0);
                ui.label(RichText::new(desc).font(body(13.5)).color(t.text_2));
                ui.add_space(10.0);
                Frame::new()
                    .fill(t.bg)
                    .corner_radius(CornerRadius::same(8))
                    .inner_margin(Margin::symmetric(12, 10))
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.label(RichText::new(&detail).font(mono(12.5)).color(t.text));
                    });

                if self.is_multi_pc_shared() {
                    ui.add_space(10.0);
                    Frame::new()
                        .fill(t.err.gamma_multiply(0.10))
                        .stroke(Stroke::new(1.0_f32, t.err.gamma_multiply(0.55)))
                        .corner_radius(CornerRadius::same(8))
                        .inner_margin(Margin::symmetric(12, 8))
                        .show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            ui.label(
                                RichText::new(
                                    "警告（複数 PC 共有）: このリポジトリは複数 PC で共有されています。削除すると他の PC のスナップショットもすべて削除されます。",
                                )
                                .font(strong(12.5))
                                .color(t.err),
                            );
                        });
                }

                if scope == R2DeleteScope::EntireBucket && !prefix.is_empty() {
                    ui.add_space(10.0);
                    Frame::new()
                        .fill(t.warn.gamma_multiply(0.10))
                        .stroke(Stroke::new(1.0_f32, t.warn.gamma_multiply(0.45)))
                        .corner_radius(CornerRadius::same(8))
                        .inner_margin(Margin::symmetric(12, 8))
                        .show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            ui.label(
                                RichText::new(format!(
                                    "注意: プレフィックス「{prefix}」以外のフォルダや他 PC のバックアップも含め、バケット全体が空になります。"
                                ))
                                .font(body(12.5))
                                .color(t.warn),
                            );
                        });
                }

                if scope == R2DeleteScope::EntireBucket {
                    ui.add_space(6.0);
                    help_text(
                        ui,
                        &t,
                        "Object Read & Write トークンの場合は中身をすべて空にした時点で完了し、バケット本体は Cloudflare ダッシュボードから削除できます。",
                    );
                }

                ui.add_space(12.0);
                ui.horizontal_wrapped(|ui| {
                    if Btn::secondary("対象件数を確認 (Dry-Run)")
                        .icon(VectorIcon::Refresh)
                        .busy(self.delete_r2_preview_loading)
                        .enabled(!self.job_running(Job::DeleteR2))
                        .show(ui, &t)
                        .clicked()
                    {
                        run_preview = true;
                    }
                    if self.delete_r2_preview_loading {
                        ui.label(
                            RichText::new("集計中…")
                                .font(body(12.5))
                                .color(t.text_3),
                        );
                    }
                });
                if let Some(preview) = &self.delete_r2_preview {
                    ui.add_space(6.0);
                    match preview {
                        Ok(rep) if rep.bucket_not_found => {
                            ui.label(
                                RichText::new("指定されたバケットは見つかりませんでした（すでに削除済みの可能性があります）")
                                    .font(body(12.5))
                                    .color(t.warn),
                            );
                        }
                        Ok(rep) => {
                            let summary = format!(
                                "対象: {} 件（{}）・未完了マルチパート {} 件",
                                format::count(rep.objects_found),
                                format::bytes(rep.bytes_found),
                                format::count(rep.multipart_uploads_found),
                            );
                            ui.label(RichText::new(summary).font(mono(12.5)).color(t.text));
                        }
                        Err(err) => {
                            ui.label(
                                RichText::new(format::summarize_error_for_ui(err, 220))
                                    .font(body(12.5))
                                    .color(t.err),
                            );
                        }
                    }
                }

                ui.add_space(14.0);
                field_label(
                    ui,
                    &t,
                    &format!("確認のためバケット名「{bucket}」を入力してください"),
                );
                ui.add(
                    input(&t, &mut self.delete_confirm_input, &bucket)
                        .font(mono(13.0))
                        .desired_width(f32::INFINITY),
                );
                ui.add_space(4.0);
                help_text(
                    ui,
                    &t,
                    "誤操作防止のため、バケット名を入力したうえで削除ボタンを 1.5 秒間長押ししてください。",
                );

                ui.add_space(18.0);
                let matched = self.delete_confirm_input.trim() == bucket;
                let hold_enabled = matched && !self.delete_r2_preview_loading;
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    let btn_label = match scope {
                        R2DeleteScope::PrefixOnly => "長押ししてプレフィックスを削除",
                        R2DeleteScope::EntireBucket => "長押ししてバケットを削除",
                    };
                    if hold_danger_button(
                        ui,
                        &t,
                        btn_label,
                        1.5,
                        VectorIcon::Trash,
                        hold_enabled,
                        &mut self.delete_hold_started,
                    ) {
                        decision = Some(true);
                    }
                    if Btn::ghost("キャンセル").show(ui, &t).clicked() {
                        decision = Some(false);
                    }
                });
            });

        if run_preview {
            self.start_r2_delete_preview(scope);
        }
        if decision.is_none() && modal.should_close() {
            decision = Some(false);
        }
        match decision {
            Some(true) => {
                self.delete_confirm_input.clear();
                self.delete_hold_started = None;
                self.delete_r2_preview = None;
                if let Some(confirm) = self.confirm.take() {
                    self.run_confirmed(confirm, ctx);
                }
            }
            Some(false) => {
                self.confirm = None;
                self.delete_confirm_input.clear();
                self.delete_hold_started = None;
                self.delete_r2_preview = None;
            }
            None => {}
        }
    }

    fn render_takeover_confirm(&mut self, ctx: &Context) {
        let Some(Confirm::TakeoverProject {
            project_key,
            label,
            mut local_path,
            mut delete_extra,
            mut save_local_backup_first,
            mut discard_confirmed,
            mut force_while_remote_active,
        }) = self.confirm.clone()
        else {
            return;
        };
        let t = self.tokens;
        let mut decision: Option<bool> = None;
        let mut run_preview = false;
        let mut open_snapshots = false;
        let preview_snapshot = self.takeover_preview.clone();
        let preview_loading = self.takeover_preview_loading;

        let modal = egui::Modal::new(Id::unique("confirm-takeover"))
            .backdrop_color(Color32::from_black_alpha(if t.is_dark { 170 } else { 90 }))
            .frame(
                Frame::new()
                    .fill(t.surface)
                    .stroke(Stroke::new(1.0_f32, t.line_strong))
                    .corner_radius(CornerRadius::same(16))
                    .inner_margin(Margin::same(26))
                    .shadow(ctx.global_style().visuals.window_shadow),
            )
            .show(ctx, |ui| {
                ui.set_width(540.0);
                ui.label(
                    RichText::new(format!("「{label}」をこの PC で引き継ぐ"))
                        .font(strong(19.0))
                        .color(t.text),
                );
                ui.add_space(4.0);
                ui.label(
                    RichText::new(format!("project:{project_key}"))
                        .font(mono(12.5))
                        .color(t.text_3),
                );
                ui.add_space(12.0);

                let prev_local_path = local_path.clone();
                field_label(ui, &t, "復元先フォルダ");
                ui.horizontal(|ui| {
                    let w = (ui.available_width() - 104.0).max(160.0);
                    ui.add(
                        input(&t, &mut local_path, r"例: D:\Projects\ProjectAlpha")
                            .font(mono(12.5))
                            .desired_width(w),
                    );
                    if Btn::secondary("参照")
                        .icon(VectorIcon::Folder)
                        .show(ui, &t)
                        .clicked()
                        && let Some(dir) = rfd::FileDialog::new()
                            .set_title("復元先フォルダを選択")
                            .pick_folder()
                    {
                        local_path = dir.to_string_lossy().into_owned();
                    }
                });
                let path_changed = local_path != prev_local_path;
                if path_changed {
                    discard_confirmed = false;
                }
                ui.add_space(8.0);
                ui.horizontal_wrapped(|ui| {
                    if Btn::secondary("最新スナップショットとローカル変更を確認")
                        .icon(VectorIcon::Refresh)
                        .busy(preview_loading && !path_changed)
                        .enabled(!local_path.trim().is_empty())
                        .show(ui, &t)
                        .clicked()
                    {
                        run_preview = true;
                    }
                    if preview_loading && !path_changed {
                        ui.label(RichText::new("確認中…").font(body(12.5)).color(t.text_3));
                    } else if path_changed {
                        ui.label(
                            RichText::new("復元先が変更されました。再度「確認」を実行してください")
                                .font(body(12.5))
                                .color(t.warn),
                        );
                    }
                });

                if !path_changed && let Some(preview) = &preview_snapshot {
                    ui.add_space(10.0);
                    match preview {
                        Ok(rep) => {
                            let when = format::parse_local(&rep.source_time)
                                .map(|at| format::day_time(at, chrono::Local::now()))
                                .unwrap_or_else(|| rep.source_time.clone());
                            let status_text = if rep.remote_released {
                                "引き渡し済み (handoff:released)"
                            } else if rep.remote_possibly_active {
                                "未引き渡し（作業中の可能性）"
                            } else {
                                "この PC のスナップショット"
                            };
                            let detail = format!(
                                "復元元: {} ({} · {when})\n状態: {status_text}\n元パス: {}\n復元先: {}",
                                rep.source_short_id, rep.source_host, rep.source_path, rep.local_path
                            );
                            Frame::new()
                                .fill(t.bg)
                                .corner_radius(CornerRadius::same(8))
                                .inner_margin(Margin::symmetric(12, 10))
                                .show(ui, |ui| {
                                    ui.set_width(ui.available_width());
                                    ui.label(RichText::new(detail).font(mono(12.0)).color(t.text));
                                });

                            for warn in &rep.warnings {
                                ui.add_space(8.0);
                                Frame::new()
                                    .fill(t.warn.gamma_multiply(0.10))
                                    .stroke(Stroke::new(1.0_f32, t.warn.gamma_multiply(0.45)))
                                    .corner_radius(CornerRadius::same(8))
                                    .inner_margin(Margin::symmetric(10, 7))
                                    .show(ui, |ui| {
                                        ui.set_width(ui.available_width());
                                        ui.label(RichText::new(warn).font(body(12.5)).color(t.warn));
                                    });
                            }

                            if rep.remote_possibly_active {
                                ui.add_space(8.0);
                                ui.checkbox(
                                    &mut force_while_remote_active,
                                    "相手 PC で作業中の可能性を理解したうえで強制引き継ぎする",
                                );
                            }

                            if rep.has_unbacked_local_changes && !rep.was_interrupted {
                                ui.add_space(10.0);
                                field_label(ui, &t, "ローカルの未バックアップ変更の扱い");
                                ui.radio_value(
                                    &mut save_local_backup_first,
                                    true,
                                    "退避スナップショットを保存してから引き継ぐ（推奨）",
                                );
                                ui.radio_value(
                                    &mut save_local_backup_first,
                                    false,
                                    "退避せずローカル変更を破棄して引き継ぐ",
                                );
                                if !save_local_backup_first {
                                    ui.add_space(4.0);
                                    ui.checkbox(
                                        &mut discard_confirmed,
                                        "ローカルの未バックアップ変更が上書き・破棄されることを確認しました",
                                    );
                                }
                                ui.add_space(4.0);
                                if crate::theme::link_button(
                                    ui,
                                    &t,
                                    "別フォルダへ通常復元する（スナップショット画面を開く）",
                                )
                                .clicked()
                                {
                                    open_snapshots = true;
                                }
                            }
                        }
                        Err(err) => {
                            Frame::new()
                                .fill(t.err.gamma_multiply(0.08))
                                .stroke(Stroke::new(1.0_f32, t.err.gamma_multiply(0.45)))
                                .corner_radius(CornerRadius::same(8))
                                .inner_margin(Margin::symmetric(10, 8))
                                .show(ui, |ui| {
                                    ui.set_width(ui.available_width());
                                    ui.label(
                                        RichText::new(format::summarize_error_for_ui(err, 320))
                                            .font(body(12.5))
                                            .color(t.err),
                                    );
                                });
                        }
                    }
                }

                ui.add_space(10.0);
                ui.checkbox(
                    &mut delete_extra,
                    "相手 PC で削除されたファイルを同期削除する (--delete、Unity 除外フォルダは保護)",
                );

                let can_execute = match &preview_snapshot {
                    Some(Ok(rep)) if !preview_loading => {
                        let remote_ok = !rep.remote_possibly_active || force_while_remote_active;
                        let local_ok = !rep.has_unbacked_local_changes
                            || rep.was_interrupted
                            || save_local_backup_first
                            || discard_confirmed;
                        remote_ok && local_ok && !local_path.trim().is_empty()
                    }
                    _ => false,
                };

                ui.add_space(18.0);
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if Btn::primary("引き継ぎを実行")
                        .icon(VectorIcon::Download)
                        .enabled(can_execute)
                        .show(ui, &t)
                        .clicked()
                    {
                        decision = Some(true);
                    }
                    if Btn::ghost("中止").show(ui, &t).clicked() {
                        decision = Some(false);
                    }
                });
            });

        let path_edited = matches!(
            &self.confirm,
            Some(Confirm::TakeoverProject { local_path: orig, .. }) if *orig != local_path
        );
        if path_edited {
            self.takeover_preview_seq = self.takeover_preview_seq.wrapping_add(1);
            self.takeover_preview = None;
            self.takeover_preview_loading = false;
            discard_confirmed = false;
        }
        if open_snapshots {
            self.takeover_preview_seq = self.takeover_preview_seq.wrapping_add(1);
            self.confirm = None;
            self.takeover_preview = None;
            self.takeover_preview_loading = false;
            self.go(AppTab::Snapshots);
            return;
        }
        if decision.is_none() && modal.should_close() {
            decision = Some(false);
        }
        match decision {
            Some(true) if !path_edited => {
                let preview_ok = preview_snapshot.as_ref().and_then(|res| res.as_ref().ok());
                let expected_source_snapshot_id =
                    preview_ok.map(|rep| rep.source_snapshot_id.clone());
                let expected_canonical_path = preview_ok.map(|rep| rep.local_path.clone());
                let pinned_local_path = expected_canonical_path
                    .clone()
                    .or_else(|| (!local_path.trim().is_empty()).then_some(local_path));
                self.takeover_preview_seq = self.takeover_preview_seq.wrapping_add(1);
                self.confirm = None;
                self.takeover_preview = None;
                self.takeover_preview_loading = false;
                self.start_takeover(TakeoverParams {
                    project_key: project_key.clone(),
                    local_path: pinned_local_path,
                    dry_run: false,
                    save_local_backup_first,
                    discard_local_changes: !save_local_backup_first && discard_confirmed,
                    force_while_remote_active,
                    delete_extra,
                    confirm_project_key: Some(project_key),
                    expected_source_snapshot_id,
                    expected_canonical_path,
                });
            }
            Some(true) => {
                self.confirm = Some(Confirm::TakeoverProject {
                    project_key,
                    label,
                    local_path,
                    delete_extra,
                    save_local_backup_first,
                    discard_confirmed: false,
                    force_while_remote_active,
                });
            }
            Some(false) => {
                self.takeover_preview_seq = self.takeover_preview_seq.wrapping_add(1);
                self.confirm = None;
                self.takeover_preview = None;
                self.takeover_preview_loading = false;
            }
            None => {
                self.confirm = Some(Confirm::TakeoverProject {
                    project_key: project_key.clone(),
                    label,
                    local_path: local_path.clone(),
                    delete_extra,
                    save_local_backup_first,
                    discard_confirmed,
                    force_while_remote_active,
                });
                if run_preview {
                    self.start_takeover_preview(
                        project_key,
                        local_path,
                        delete_extra,
                        save_local_backup_first,
                    );
                }
            }
        }
    }

    fn handle_screenshot(&mut self, ctx: &Context) {
        let Some(path) = self.screenshot_path.clone() else {
            return;
        };
        if !self.screenshot_requested
            && self.frame_count >= 10
            && self.started_instant.elapsed() >= Duration::from_millis(900)
        {
            self.screenshot_requested = true;
            let controller = Arc::clone(&self.controller);
            ctx.request_screenshot(move |image| {
                if let Some(parent) = path.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                let [w, h] = image.size;
                let rgba: Vec<u8> = image.pixels.iter().flat_map(|c| c.to_array()).collect();
                let saved =
                    image::save_buffer(&path, &rgba, w as u32, h as u32, image::ColorType::Rgba8);
                // The next frame sees the quit request and closes the window.
                if saved.is_ok() {
                    controller.request_quit();
                }
            });
        }
        ctx.request_repaint();
    }
}

impl DesignTokens {
    /// Ink used inside the lime brand tile, identical in both themes.
    fn on_primary_mark(&self) -> Color32 {
        Color32::from_rgb(0x0E, 0x0E, 0x0D)
    }
}

fn format_forget_preview_for_ui(raw_json: &str) -> String {
    let Ok(val) = serde_json::from_str::<serde_json::Value>(raw_json) else {
        return raw_json.to_owned();
    };
    if val.get("skipped").and_then(serde_json::Value::as_bool) == Some(true) {
        return "保持数がすべて 0 のため、削除対象のスナップショットはありません。".to_owned();
    }
    let keep = val
        .get("totalKeep")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    let remove = val
        .get("totalRemove")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    let safety = val
        .get("protectedSafetyBackups")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    let mut lines = vec![format!(
        "確認結果 (Dry-Run): 保持 {keep} 件 / 削除候補 {remove} 件（退避保護 {safety} 件）"
    )];
    let append_groups = |lines: &mut Vec<String>, title: &str, key: &str| {
        if let Some(arr) = val.get(key).and_then(serde_json::Value::as_array)
            && !arr.is_empty()
        {
            lines.push(format!("[{title}]"));
            for g in arr {
                let label = g
                    .get("groupLabel")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("-");
                let gk = g
                    .get("keepCount")
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or(0);
                let gr = g
                    .get("removeCount")
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or(0);
                let gs = g
                    .get("protectedSafetyCount")
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or(0);
                let ids: Vec<&str> = g
                    .get("removeShortIds")
                    .and_then(serde_json::Value::as_array)
                    .map(|a| a.iter().filter_map(serde_json::Value::as_str).collect())
                    .unwrap_or_default();
                let id_suffix = if ids.is_empty() {
                    String::new()
                } else {
                    format!(" → 削除候補: {}", ids.join(", "))
                };
                let safety_suffix = if gs > 0 {
                    format!("・退避保護 {gs}")
                } else {
                    String::new()
                };
                lines.push(format!(
                    "  {label}: 保持 {gk}{safety_suffix} / 削除 {gr}{id_suffix}"
                ));
            }
        }
    };
    append_groups(&mut lines, "共有プロジェクト", "sharedProjects");
    append_groups(&mut lines, "旧形式・ローカル専用", "legacyGroups");
    lines.join("\n")
}

fn load_history(ctrl: &SharedController) -> (Vec<BackupRunRecord>, Vec<AuditLogRecord>) {
    // 1000 runs cover the 7-day coverage grid even at a 15-minute interval.
    HistoryDb::open(&ctrl.db_path)
        .map(|db| {
            (
                db.recent_backup_runs(1000).unwrap_or_default(),
                db.recent_audit_logs(200).unwrap_or_default(),
            )
        })
        .unwrap_or_default()
}

impl eframe::App for ResticTrayApp {
    /// Window state and background polling. eframe calls this before every `ui`, and on its
    /// own while the window is minimized, which is when it needs to be moved to the tray.
    fn logic(&mut self, ctx: &Context, _frame: &mut eframe::Frame) {
        self.frame_count = self.frame_count.saturating_add(1);

        // eframe shows the window after painting the first frame even when it was built with
        // `with_visible(false)`, so a tray start hides it again on the next frame.
        if self.start_hidden {
            if self.frame_count >= 2 {
                self.start_hidden = false;
                self.in_tray = true;
                ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
            } else {
                ctx.request_repaint();
            }
        }
        let show_requested = self.controller.take_show_window_request();
        if show_requested {
            self.start_hidden = false;
            self.in_tray = false;
            ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
            ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
        }
        // Quit from the tray menu is handled outside the frame loop; the window closes here.
        if self.controller.is_quit_requested() {
            crate::rclone::shutdown();
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
        // Closing or minimizing the window keeps the app (scheduler + MCP bridge) alive in the tray.
        let (close_requested, minimized) = ctx.input(|i| {
            (
                i.viewport().close_requested(),
                i.viewport().minimized == Some(true),
            )
        });
        if (close_requested || minimized)
            && !self.in_tray
            && !show_requested
            && !self.controller.is_quit_requested()
            && self.screenshot_path.is_none()
        {
            if close_requested {
                ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            }
            self.in_tray = true;
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
        }
        // Nothing below may run while in the tray: any repaint request keeps the frame loop
        // spinning for a window nobody sees. Showing the window starts it again.
        if self.in_tray {
            return;
        }

        self.drain_events();

        if !self.startup_sync_triggered
            && self.screenshot_path.is_none()
            && self.status.storage_configured
            && self.status.restic_binary.available
            && self.status.secrets_readiness.is_complete()
        {
            self.startup_sync_triggered = true;
            self.refresh_sync_status_async(false);
        }

        let poll_every = if self.backup_active()
            || !self.jobs.is_empty()
            || self.delete_r2_preview_loading
            || self
                .status
                .active_r2_delete
                .as_ref()
                .is_some_and(|p| p.running)
        {
            Duration::from_millis(900)
        } else {
            Duration::from_secs(4)
        };
        if (self.poll_soon || self.last_poll.elapsed() >= poll_every) && !self.poll_in_flight {
            self.poll_soon = false;
            self.refresh_telemetry_async();
        }

        self.handle_screenshot(ctx);
        if self.screenshot_path.is_none() {
            // Keeps job events and telemetry flowing while the window is shown.
            ctx.request_repaint_after(Duration::from_millis(500));
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        if self.in_tray {
            // Drawing would restart animation repaints. The fill is what the window
            // shows for a moment when it comes back, before the next real frame.
            ui.painter()
                .rect_filled(ui.max_rect(), CornerRadius::ZERO, self.tokens.bg);
            return;
        }
        let ctx = &ui.ctx().clone();
        self.handle_shortcuts(ctx);
        self.handle_dropped_folders(ctx);

        self.render_sidebar(ui);

        let t = self.tokens;
        egui::CentralPanel::default()
            .frame(Frame::new().fill(t.bg))
            .show(ui, |ui| {
                let now = ui.input(|i| i.time);
                if self.page_shown_at.is_nan() {
                    self.page_shown_at = now;
                }
                if self.page == AppTab::Settings {
                    self.settings_save_bar(ui);
                } else {
                    self.settings_dirty = false;
                }
                let k = (((now - self.page_shown_at) / 0.2) as f32).clamp(0.0, 1.0);
                if k < 1.0 {
                    ui.ctx().request_repaint();
                }
                let eased = 1.0 - (1.0 - k).powi(3);
                egui::ScrollArea::vertical()
                    .id_salt(("page", self.page as u8))
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        ui.multiply_opacity(eased);
                        Frame::new()
                            .inner_margin(Margin {
                                left: 44,
                                right: 44,
                                top: 36 + ((1.0 - eased) * 12.0) as i8,
                                bottom: 48,
                            })
                            .show(ui, |ui| {
                                ui.set_width(ui.available_width().min(CONTENT_MAX_WIDTH));
                                match self.page {
                                    AppTab::Dashboard => self.page_overview(ui),
                                    AppTab::Targets => self.page_targets(ui),
                                    AppTab::Snapshots => self.page_snapshots(ui),
                                    AppTab::History => self.page_history(ui),
                                    AppTab::Settings => self.page_settings(ui),
                                }
                            });
                    });
            });

        self.render_drop_overlay(ctx);
        self.render_toasts(ctx);
        self.render_confirm(ctx);
    }
}

/// Allocates a full-width list row and returns `(rect, response)`; callers paint the cells.
/// Only rows that do something on click should pass `Sense::click()`, because clickable
/// rows get the pointer cursor and an accessible action.
fn list_row(
    ui: &mut egui::Ui,
    height: f32,
    id: impl std::hash::Hash + std::fmt::Debug,
    sense: Sense,
) -> (Rect, egui::Response) {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), height), Sense::hover());
    let response = ui.interact(rect, ui.scope_id().with(id), sense);
    (rect, response)
}

/// Runs `add` in a Ui pinned to `rect` without moving the parent's cursor. `scope_builder`
/// would advance the cursor to the end of `rect`, which can be above the end of a list row.
fn place<R>(
    ui: &mut egui::Ui,
    rect: Rect,
    layout: Layout,
    add: impl FnOnce(&mut egui::Ui) -> R,
) -> R {
    let mut child = ui.new_child(egui::UiBuilder::new().max_rect(rect).layout(layout));
    add(&mut child)
}

/// Paints `text` left-aligned in a cell, elided to the cell width.
fn cell(
    ui: &egui::Ui,
    pos: Pos2,
    width: f32,
    text: &str,
    font: egui::FontId,
    color: Color32,
) -> Rect {
    let galley = theme::elided(ui, text, font, color, width);
    let rect = Rect::from_min_size(
        Pos2::new(pos.x, pos.y - galley.size().y / 2.0),
        galley.size(),
    );
    ui.painter().galley(rect.min, galley, color);
    rect
}

/// Paints `text` right-aligned so its right edge sits at `right`.
fn cell_right(ui: &egui::Ui, right: f32, y: f32, text: &str, font: egui::FontId, color: Color32) {
    ui.painter()
        .text(Pos2::new(right, y), Align2::RIGHT_CENTER, text, font, color);
}

fn run_status_color(t: &DesignTokens, run: &BackupRunRecord) -> Color32 {
    if run.status == "deferred" {
        t.warn
    } else if run.status != "success" {
        t.err
    } else if run.dry_run {
        t.text_3
    } else if run.error_message.is_some() {
        // Saved, but some files could not be read.
        t.warn
    } else {
        t.ok()
    }
}

fn spinner_inline(ui: &mut egui::Ui, t: &DesignTokens, label: &str) {
    ui.horizontal(|ui| {
        let (r, _) = ui.allocate_exact_size(Vec2::splat(16.0), Sense::hover());
        paint_spinner(ui, r.center(), 6.0, t.text_2);
        ui.label(RichText::new(label).font(body(13.0)).color(t.text_2));
    });
}

fn cancel_backup(ctrl: &SharedController) {
    ctrl.cancel_flag.store(true, Ordering::Relaxed);
}
