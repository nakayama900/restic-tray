//! Windows system tray icon and context menu integration (`tray-icon`).
//!
//! Keeps a resident tray icon with quick actions:
//! - Open the window
//! - Run a backup now
//! - Run a dry run (test without uploading)
//! - Open `config.json`
//! - Quit Restic Tray
//!
//! Events are handled in tray-icon's event handlers, not polled from the frame loop: the app
//! keeps that loop idle while the window is hidden, which is exactly when the tray is used.

use std::sync::Arc;

use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder, TrayIconEvent};

use crate::controller::SharedController;
use crate::window;

pub struct SystemTrayHandle {
    _tray: TrayIcon,
}

impl SystemTrayHandle {
    pub fn try_create(controller: &Arc<SharedController>) -> Option<Self> {
        let icon = load_embedded_tray_icon()?;
        let menu = Menu::new();

        let item_open = MenuItem::new("ウィンドウを開く", true, None);
        let item_backup_now = MenuItem::new("今すぐバックアップ", true, None);
        let item_dry_run = MenuItem::new("テスト実行（送信しない）", true, None);
        let item_open_config = MenuItem::new("config.json を開く", true, None);
        let item_quit = MenuItem::new("終了", true, None);

        let _ = menu.append(&item_open);
        let _ = menu.append(&PredefinedMenuItem::separator());
        let _ = menu.append(&item_backup_now);
        let _ = menu.append(&item_dry_run);
        let _ = menu.append(&PredefinedMenuItem::separator());
        let _ = menu.append(&item_open_config);
        let _ = menu.append(&PredefinedMenuItem::separator());
        let _ = menu.append(&item_quit);

        let tray = TrayIconBuilder::new()
            .with_tooltip("Restic Tray")
            .with_icon(icon)
            .with_menu(Box::new(menu))
            .build()
            .ok()?;

        let ctrl = Arc::clone(controller);
        TrayIconEvent::set_event_handler(Some(move |event: TrayIconEvent| {
            if let TrayIconEvent::Click {
                button: tray_icon::MouseButton::Left,
                ..
            }
            | TrayIconEvent::DoubleClick { .. } = event
            {
                show(&ctrl);
            }
        }));

        let ctrl = Arc::clone(controller);
        let (id_open, id_backup_now, id_dry_run, id_open_config, id_quit) = (
            item_open.id().clone(),
            item_backup_now.id().clone(),
            item_dry_run.id().clone(),
            item_open_config.id().clone(),
            item_quit.id().clone(),
        );
        MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
            if event.id == id_open {
                show(&ctrl);
            } else if event.id == id_backup_now || event.id == id_dry_run {
                let ctrl = Arc::clone(&ctrl);
                let dry_run = event.id == id_dry_run;
                std::thread::spawn(move || {
                    let _ = ctrl.execute_backup("tray", None, dry_run, &[]);
                });
            } else if event.id == id_open_config {
                let _ = open::that(&ctrl.config_path);
            } else if event.id == id_quit {
                ctrl.request_quit();
                // `logic` closes the window once it runs, which needs the window visible.
                window::show_main();
            }
        }));

        Some(Self { _tray: tray })
    }
}

/// Shows the window now and lets `logic` sync egui's visibility state on its next frame.
fn show(controller: &SharedController) {
    controller.request_show_window();
    window::show_main();
}

fn load_embedded_tray_icon() -> Option<Icon> {
    let png_bytes = include_bytes!("../assets/icon.png");
    let img = image::load_from_memory(png_bytes).ok()?.into_rgba8();
    let (w, h) = img.dimensions();
    Icon::from_rgba(img.into_raw(), w, h).ok()
}
