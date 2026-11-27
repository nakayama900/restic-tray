//! One resident app per data folder.
//!
//! A second launch (e.g. from the Start menu while the first sits in the tray) asks the
//! running app to show its window and exits. Two instances would each run the scheduler,
//! so the same 5:00 backup would start twice and one would fail on the restic lock, and
//! each would save its own copy of `config.json` over the other's.

use std::sync::Arc;

use crate::controller::SharedController;

/// Returns `true` when this process should keep running as the resident app, or `false`
/// when another instance is running and was asked to show its window.
#[cfg(windows)]
pub fn claim_or_activate(controller: &Arc<SharedController>) -> bool {
    use std::sync::mpsc;

    let name = pipe_name(&controller.base_dir);
    let (tx, rx) = mpsc::channel();
    let ctrl = Arc::clone(controller);
    let spawned = std::thread::Builder::new()
        .name("restic-tray-instance".to_owned())
        .spawn(move || {
            let rt = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(err) => {
                    tracing::warn!("single-instance check skipped: {err}");
                    let _ = tx.send(true);
                    return;
                }
            };
            rt.block_on(windows_impl::claim(&name, &ctrl, tx));
        });
    // Failures here fail open: running twice is better than not running at all.
    spawned.is_err() || rx.recv().unwrap_or(true)
}

#[cfg(not(windows))]
pub fn claim_or_activate(_controller: &Arc<SharedController>) -> bool {
    true
}

/// Fixed per data folder, so an isolated `LOCALAPPDATA` (tests, screenshots) gets its own.
fn pipe_name(base_dir: &std::path::Path) -> String {
    // FNV-1a: stable across builds, unlike std's `DefaultHasher`.
    let hash = base_dir
        .to_string_lossy()
        .to_lowercase()
        .bytes()
        .fold(0xcbf2_9ce4_8422_2325_u64, |h, b| {
            (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
        });
    format!(r"\\.\pipe\restic-tray-instance-{hash:016x}")
}

#[cfg(windows)]
mod windows_impl {
    use std::sync::mpsc::Sender;
    use std::time::Duration;

    use tokio::net::windows::named_pipe::{ClientOptions, NamedPipeServer, ServerOptions};

    use crate::controller::SharedController;

    /// `FILE_FLAG_FIRST_PIPE_INSTANCE` fails with this when another process owns the name.
    const ERROR_ACCESS_DENIED: i32 = 5;
    /// All server instances are busy; the running app is between connections.
    const ERROR_PIPE_BUSY: i32 = 231;

    pub(super) async fn claim(
        name: &str,
        controller: &SharedController,
        keep_running: Sender<bool>,
    ) {
        match create(name, true) {
            Ok(server) => {
                let _ = keep_running.send(true);
                listen(server, name, controller).await;
            }
            Err(err) if err.raw_os_error() == Some(ERROR_ACCESS_DENIED) => {
                let _ = keep_running.send(!activate_running(name).await);
            }
            Err(err) => {
                tracing::warn!("single-instance check skipped: {err}");
                let _ = keep_running.send(true);
            }
        }
    }

    fn create(name: &str, first: bool) -> std::io::Result<NamedPipeServer> {
        ServerOptions::new()
            .first_pipe_instance(first)
            .reject_remote_clients(true)
            .create(name)
    }

    /// Any connection is a request to show the window; nothing is read from it.
    async fn listen(mut server: NamedPipeServer, name: &str, controller: &SharedController) {
        loop {
            let connected = server.connect().await.is_ok();
            // Create the next instance before the used one is dropped, so the name is never
            // free for a moment in which a new launch could claim it.
            server = match create(name, false) {
                Ok(next) => next,
                Err(err) => {
                    tracing::warn!("single-instance pipe stopped: {err}");
                    return;
                }
            };
            if connected {
                controller.request_show_window();
                crate::window::show_main();
            }
        }
    }

    async fn activate_running(name: &str) -> bool {
        crate::window::allow_any_foreground();
        for _ in 0..40 {
            match ClientOptions::new().open(name) {
                Ok(client) => {
                    // Stay connected briefly so the server sees the connection before it closes.
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    drop(client);
                    return true;
                }
                Err(err) if err.raw_os_error() == Some(ERROR_PIPE_BUSY) => {
                    tokio::time::sleep(Duration::from_millis(25)).await;
                }
                Err(err) => {
                    tracing::warn!("could not reach the running instance: {err}");
                    return false;
                }
            }
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pipe_name_is_per_data_folder_and_ignores_case() {
        let a = pipe_name(std::path::Path::new(r"C:\Users\a\AppData\Local\x"));
        assert_eq!(
            a,
            pipe_name(std::path::Path::new(r"c:\users\A\appdata\local\X"))
        );
        assert_ne!(
            a,
            pipe_name(std::path::Path::new(r"C:\Users\b\AppData\Local\x"))
        );
    }

    #[cfg(windows)]
    #[test]
    fn claim_or_activate_lets_a_second_launch_show_the_first_window_and_exit() {
        use std::time::{Duration, Instant};

        let dir =
            std::env::temp_dir().join(format!("restic-tray-instance-test-{}", std::process::id()));
        let first = SharedController::new(dir.clone(), dir.join("config.json"))
            .expect("controller in a temp dir");
        let second = SharedController::new(dir.clone(), dir.join("config.json"))
            .expect("controller in a temp dir");

        assert!(claim_or_activate(&first));
        assert!(!first.take_show_window_request());
        assert!(!claim_or_activate(&second));

        let deadline = Instant::now() + Duration::from_secs(3);
        while !first.take_show_window_request() {
            assert!(
                Instant::now() < deadline,
                "first instance never got the show request"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
