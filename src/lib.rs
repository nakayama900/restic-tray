pub mod agent;
pub mod app;
pub mod config;
pub mod controller;
pub mod db;
pub mod error;
pub mod fs_safety;
pub mod instance;
pub mod notify;
pub mod r2_s3;
pub mod rclone;
pub mod restic;
pub mod scheduler;
pub mod secrets;
pub mod sync;
pub mod theme;
pub mod tray;
pub mod window;

use std::path::PathBuf;

use app::{AppTab, ResticTrayApp};
use config::{default_app_data_dir, default_config_path};
use controller::SharedController;
use scheduler::spawn_scheduler_thread;

#[derive(Debug, Default)]
pub struct LaunchOptions {
    pub config_path: Option<PathBuf>,
    pub initial_tab: Option<AppTab>,
    pub initial_dark: Option<bool>,
    pub start_minimized: bool,
    pub screenshot_path: Option<PathBuf>,
}

/// Deletes the copies of this exe that an update moved aside because they were running
/// (`<exe name>.<token>.old`, see `packaging/nsis/installer.nsi`). A copy still in use cannot
/// be deleted and is left for a later start.
fn remove_replaced_binaries(exe: &std::path::Path) {
    let (Some(dir), Some(name)) = (exe.parent(), exe.file_name().and_then(|n| n.to_str())) else {
        return;
    };
    let prefix = format!("{name}.");
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let file = entry.file_name();
        let file = file.to_string_lossy();
        if file.starts_with(&prefix) && file.ends_with(".old") {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

pub(crate) fn startup_error_dialog_content(err: &error::AppError) -> (&'static str, String) {
    match err {
        error::AppError::Config(_) => ("Restic Tray — 設定エラー", err.to_string()),
        other => (
            "Restic Tray — 起動エラー",
            format!("Restic Tray を起動できませんでした。\n\n{other}"),
        ),
    }
}

pub fn run_gui(opts: LaunchOptions) -> eframe::Result<()> {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .try_init();

    let base_dir = default_app_data_dir();
    let cfg_path = opts.config_path.unwrap_or_else(default_config_path);
    let controller = match SharedController::new(base_dir, cfg_path) {
        Ok(ctrl) => ctrl,
        Err(err) => {
            tracing::error!("Failed to initialize Restic Tray controller: {err}");
            let (title, description) = startup_error_dialog_content(&err);
            let _ = rfd::MessageDialog::new()
                .set_level(rfd::MessageLevel::Error)
                .set_title(title)
                .set_description(&description)
                .set_buttons(rfd::MessageButtons::Ok)
                .show();
            return Err(eframe::Error::AppCreation(Box::new(err)));
        }
    };

    if !instance::claim_or_activate(&controller) {
        tracing::info!("Restic Tray is already running; asked it to show its window");
        return Ok(());
    }

    if let Ok(exe) = std::env::current_exe() {
        remove_replaced_binaries(&exe);
    }

    spawn_scheduler_thread(std::sync::Arc::clone(&controller));
    agent::bridge::spawn_agent_bridge_thread(std::sync::Arc::clone(&controller));

    let cfg = controller.get_config();
    let start_hidden =
        (opts.start_minimized || cfg.start_minimized_to_tray) && opts.screenshot_path.is_none();

    let mut viewport = egui::ViewportBuilder::default()
        .with_title("Restic Tray")
        .with_inner_size([1240.0, 820.0])
        .with_min_inner_size([980.0, 640.0])
        .with_visible(!start_hidden);

    if let Ok(icon) = eframe::icon_data::from_png_bytes(include_bytes!("../assets/icon.png")) {
        viewport = viewport.with_icon(std::sync::Arc::new(icon));
    }

    let native_options = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };

    let initial_tab = opts.initial_tab.unwrap_or(AppTab::Dashboard);
    let initial_dark = opts.initial_dark;
    let screenshot_path = opts.screenshot_path;

    eframe::run_native(
        "Restic Tray",
        native_options,
        Box::new(move |cc| {
            Ok(Box::new(ResticTrayApp::new(
                cc,
                controller,
                initial_tab,
                initial_dark,
                start_hidden,
                screenshot_path,
            )))
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remove_replaced_binaries_deletes_only_this_exe_s_moved_aside_copies() {
        let dir = tempfile::tempdir().expect("tempdir");
        let names = [
            "app.exe",
            "app.exe.nsA1B2.tmp.old",
            "app.exe.0.2.1.old",
            "app.exe.old-085937",
            "restic.exe.x.old",
        ];
        for name in names {
            std::fs::write(dir.path().join(name), b"x").expect("write");
        }

        remove_replaced_binaries(&dir.path().join("app.exe"));

        let mut left: Vec<String> = std::fs::read_dir(dir.path())
            .expect("read dir")
            .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
            .collect();
        left.sort();
        assert_eq!(left, ["app.exe", "app.exe.old-085937", "restic.exe.x.old"]);
    }
}
