//! Sidecar `restic.exe` path resolution with PATH fallback.
//!
//! Probes in order:
//! 1. Explicit override path in `config.json` (`resticBinaryPath`)
//! 2. `<exe_dir>/restic.exe` or `<exe_dir>/binaries/restic.exe`
//! 3. `<cwd>/binaries/restic.exe` (development checkout)
//! 4. Ambient `restic.exe` on `PATH`

use std::path::{Path, PathBuf};
use std::process::Command;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResticBinaryInfo {
    pub resolved_path: Option<String>,
    pub source: String,
    pub version_output: Option<String>,
    pub available: bool,
}

pub fn sidecar_filenames() -> [&'static str; 2] {
    #[cfg(windows)]
    return ["restic.exe", "restic-x86_64-pc-windows-msvc.exe"];
    #[cfg(not(windows))]
    return ["restic", "restic-x86_64-unknown-linux-gnu"];
}

/// Resolve the path to `restic.exe` and describe where it was found.
pub fn resolve_restic_path(configured_override: Option<&str>) -> (PathBuf, &'static str) {
    let names = sidecar_filenames();
    if let Some(custom) = configured_override.map(str::trim).filter(|s| !s.is_empty()) {
        let p = PathBuf::from(custom);
        if p.is_file() {
            return (p, "config_override");
        }
    }

    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf));
    let cwd = std::env::current_dir().ok();

    let candidate_dirs = [
        exe_dir.clone(),
        exe_dir.as_ref().map(|d| d.join("binaries")),
        cwd.as_ref().map(|d| d.join("binaries")),
    ];

    if let Some(found) = find_sidecar_in_dirs(candidate_dirs.into_iter().flatten()) {
        return (found, "sidecar");
    }

    (PathBuf::from(names[0]), "path_fallback")
}

pub fn find_sidecar_in_dirs(dirs: impl IntoIterator<Item = PathBuf>) -> Option<PathBuf> {
    let names = sidecar_filenames();
    dirs.into_iter()
        .flat_map(|dir| names.map(move |name| dir.join(name)))
        .find(|candidate| candidate.is_file())
}

/// Probe `restic version` without opening a console window.
pub fn probe_restic_binary(configured_override: Option<&str>) -> ResticBinaryInfo {
    let (bin_path, source) = resolve_restic_path(configured_override);
    let mut cmd = Command::new(&bin_path);
    cmd.arg("version");
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }

    match cmd.output() {
        Ok(out) if out.status.success() => {
            let stdout = String::from_utf8_lossy(&out.stdout);
            ResticBinaryInfo {
                resolved_path: Some(bin_path.to_string_lossy().into_owned()),
                source: source.to_owned(),
                version_output: Some(stdout.trim().to_owned()),
                available: true,
            }
        }
        _ => ResticBinaryInfo {
            resolved_path: if source == "path_fallback" {
                None
            } else {
                Some(bin_path.to_string_lossy().into_owned())
            },
            source: source.to_owned(),
            version_output: None,
            available: false,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn find_sidecar_locates_bundled_restic_binary() {
        let dir = tempfile::tempdir().expect("tempdir");
        let bin = dir.path().join(sidecar_filenames()[0]);
        std::fs::write(&bin, b"stub").expect("write stub");
        assert_eq!(find_sidecar_in_dirs([dir.path().to_path_buf()]), Some(bin));
    }

    #[test]
    fn find_sidecar_returns_none_when_empty() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert_eq!(find_sidecar_in_dirs([dir.path().to_path_buf()]), None);
    }
}
