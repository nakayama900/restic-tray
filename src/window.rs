//! Shows the main window from outside the egui frame loop.
//!
//! The frame loop only runs when something requests a repaint. In the tray, the app
//! requests no repaints so it uses no CPU, and Windows sends no `WM_PAINT` to a hidden
//! window. As a result, neither the tray icon nor a second launch can bring the window
//! back through egui. These calls go directly to Win32 using the handle saved at startup;
//! the frame loop resumes as soon as the window is visible.

#[cfg(windows)]
use std::sync::atomic::{AtomicIsize, Ordering};

/// The main window's `HWND`, or 0 before the window exists.
#[cfg(windows)]
static MAIN_WINDOW: AtomicIsize = AtomicIsize::new(0);

/// Remembers the main window. Call once with the eframe creation context.
#[cfg(windows)]
pub fn register(window: &impl raw_window_handle::HasWindowHandle) {
    if let Ok(handle) = window.window_handle()
        && let raw_window_handle::RawWindowHandle::Win32(win32) = handle.as_raw()
    {
        MAIN_WINDOW.store(win32.hwnd.get(), Ordering::Relaxed);
    }
}

/// Shows, restores and focuses the main window. Callable from any thread.
#[cfg(windows)]
#[allow(unsafe_code)]
pub fn show_main() {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        IsIconic, SW_RESTORE, SW_SHOW, SetForegroundWindow, ShowWindow,
    };

    let hwnd = MAIN_WINDOW.load(Ordering::Relaxed);
    if hwnd == 0 {
        return;
    }
    let hwnd = hwnd as windows_sys::Win32::Foundation::HWND;
    // SAFETY: These calls take the handle by value and only send messages to the window;
    // nothing is read or written through it. The handle is this process's own main window,
    // which lives until the process exits, and a stale handle only makes the calls fail.
    unsafe {
        ShowWindow(
            hwnd,
            if IsIconic(hwnd) != 0 {
                SW_RESTORE
            } else {
                SW_SHOW
            },
        );
        SetForegroundWindow(hwnd);
    }
}

/// Lets the already running instance take the foreground when this launch signals it.
/// Windows only lets the process the user just started change the foreground window.
#[cfg(windows)]
#[allow(unsafe_code)]
pub fn allow_any_foreground() {
    use windows_sys::Win32::UI::WindowsAndMessaging::{ASFW_ANY, AllowSetForegroundWindow};

    // SAFETY: Takes a plain integer and only changes this process's foreground permission.
    unsafe {
        AllowSetForegroundWindow(ASFW_ANY);
    }
}

#[cfg(not(windows))]
pub fn register<T>(_window: &T) {}

#[cfg(not(windows))]
pub fn show_main() {}

#[cfg(not(windows))]
pub fn allow_any_foreground() {}
