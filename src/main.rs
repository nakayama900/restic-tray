// Prevents an extra console window on Windows in release builds.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::path::PathBuf;

use restic_tray::LaunchOptions;
use restic_tray::app::AppTab;

fn main() -> eframe::Result<()> {
    let args: Vec<String> = std::env::args().collect();

    if args.iter().any(|a| a == "--version" || a == "-V") {
        println!("restic-tray {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }

    let mut config_path: Option<PathBuf> = None;
    let mut initial_tab: Option<AppTab> = None;
    let mut initial_dark: Option<bool> = None;
    let mut start_minimized = false;
    let mut screenshot_path: Option<PathBuf> = None;
    let mut mcp_mode = false;

    let mut idx = 1;
    while idx < args.len() {
        match args[idx].as_str() {
            "--mcp" => {
                mcp_mode = true;
                idx += 1;
            }
            "--minimized" | "--tray" => {
                start_minimized = true;
                idx += 1;
            }
            "--config" if idx + 1 < args.len() => {
                config_path = Some(PathBuf::from(&args[idx + 1]));
                idx += 2;
            }
            "--tab" if idx + 1 < args.len() => {
                initial_tab = Some(match args[idx + 1].as_str() {
                    "1" | "dashboard" => AppTab::Dashboard,
                    "2" | "targets" => AppTab::Targets,
                    "3" | "snapshots" => AppTab::Snapshots,
                    "4" | "history" => AppTab::History,
                    "5" | "settings" => AppTab::Settings,
                    _ => AppTab::Dashboard,
                });
                idx += 2;
            }
            "--light" => {
                initial_dark = Some(false);
                idx += 1;
            }
            "--dark" => {
                initial_dark = Some(true);
                idx += 1;
            }
            "--screenshot" if idx + 1 < args.len() => {
                screenshot_path = Some(PathBuf::from(&args[idx + 1]));
                idx += 2;
            }
            _ => {
                idx += 1;
            }
        }
    }

    if mcp_mode {
        let code = restic_tray::agent::run_mcp_stdio(config_path);
        restic_tray::rclone::shutdown();
        std::process::exit(code);
    }

    restic_tray::run_gui(LaunchOptions {
        config_path,
        initial_tab,
        initial_dark,
        start_minimized,
        screenshot_path,
    })
}
