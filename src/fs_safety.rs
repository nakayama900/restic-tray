//! Filesystem safety helpers for backup targets and restore destinations.
//!
//! Canonicalizes paths via `dunce::canonicalize` so Windows verbatim UNC prefixes
//! (`\\?\`), symlinks, and junctions cannot bypass scope or overwrite checks.

use std::path::{Path, PathBuf};

use crate::error::{AppError, AppResult};

/// Canonicalize an existing directory and verify it is a real directory.
pub fn canonicalize_existing_dir(path: &Path, label: &str) -> AppResult<PathBuf> {
    if path_contains_parent_dir(path) {
        return Err(AppError::InvalidArgument(format!(
            "{label} must not contain parent traversal ('..'): {}",
            path.display()
        )));
    }
    let canonical = dunce::canonicalize(path).map_err(|err| {
        AppError::InvalidArgument(format!(
            "{label} does not exist or cannot be resolved: {err} ({})",
            path.display()
        ))
    })?;
    if !canonical.is_dir() {
        return Err(AppError::InvalidArgument(format!(
            "{label} is not a directory: {}",
            canonical.display()
        )));
    }
    Ok(canonical)
}

/// Validate and resolve a restore destination directory.
///
/// The destination itself may not exist yet (if its parent exists), but:
/// - It must not contain `..` traversal
/// - It must not traverse an unresolved reparse point
/// - It must not be a drive root (`C:\`, `D:\`) or a protected Windows system tree
pub fn validate_restore_destination(path: &Path) -> AppResult<PathBuf> {
    validate_restore_destination_with_roots(path, &ProtectedRoots::from_env())
}

fn validate_restore_destination_with_roots(
    path: &Path,
    roots: &ProtectedRoots,
) -> AppResult<PathBuf> {
    if path.as_os_str().is_empty() {
        return Err(AppError::InvalidArgument(
            "restore destination path cannot be empty".to_owned(),
        ));
    }
    if path_contains_parent_dir(path) {
        return Err(AppError::InvalidArgument(format!(
            "refusing to restore into path with parent traversal ('..'): {}",
            path.display()
        )));
    }

    let resolved = resolve_existing_or_parent(path)?;
    if is_protected_system_path_with_roots(&resolved, roots) {
        return Err(AppError::InvalidArgument(format!(
            "refusing to restore directly into protected root or system path: {}",
            resolved.display()
        )));
    }
    Ok(resolved)
}

fn path_contains_parent_dir(path: &Path) -> bool {
    path.components()
        .any(|c| matches!(c, std::path::Component::ParentDir))
}

#[derive(Debug, Clone)]
struct ProtectedRoots {
    /// Exact directory and all descendants are protected (e.g. Windows, Program Files).
    subtree_roots: Vec<Vec<String>>,
    /// Exact directory is protected, while user/app subdirectories inside are allowed.
    exact_only_roots: Vec<Vec<String>>,
}

impl ProtectedRoots {
    fn from_env() -> Self {
        if !cfg!(windows) {
            return Self::unix_defaults();
        }
        let system_root = std::env::var("SystemRoot")
            .or_else(|_| std::env::var("WINDIR"))
            .ok();
        let system_drive = std::env::var("SystemDrive")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .or_else(|| {
                system_root
                    .as_deref()
                    .and_then(extract_drive_prefix)
                    .map(str::to_owned)
            });
        let program_files = std::env::var("ProgramFiles").ok();
        let program_files_x86 = std::env::var("ProgramFiles(x86)").ok();
        let program_w6432 = std::env::var("ProgramW6432").ok();
        let program_data = std::env::var("ProgramData").ok();
        let user_profile = std::env::var("USERPROFILE").ok();

        Self::from_values(
            system_drive.as_deref(),
            system_root.as_deref(),
            program_files.as_deref(),
            program_files_x86.as_deref(),
            program_w6432.as_deref(),
            program_data.as_deref(),
            user_profile.as_deref(),
        )
    }

    /// macOS / Linux protected restore destinations: OS and system trees. The filesystem root is
    /// rejected separately by the "no parent directory" rule.
    fn unix_defaults() -> Self {
        let subtree: &[&str] = if cfg!(target_os = "macos") {
            &["/System", "/usr", "/etc", "/private/etc", "/Library/System"]
        } else {
            &[
                "/usr", "/etc", "/boot", "/bin", "/sbin", "/lib", "/lib64", "/var",
            ]
        };
        let mut subtree_roots = Vec::new();
        for raw in subtree {
            push_unique_components(&mut subtree_roots, raw);
        }
        Self {
            subtree_roots,
            exact_only_roots: Vec::new(),
        }
    }

    fn from_values(
        system_drive: Option<&str>,
        system_root: Option<&str>,
        program_files: Option<&str>,
        program_files_x86: Option<&str>,
        program_w6432: Option<&str>,
        program_data: Option<&str>,
        user_profile: Option<&str>,
    ) -> Self {
        let drive = system_drive
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .or_else(|| system_root.and_then(extract_drive_prefix))
            .unwrap_or("C:");
        let drive_prefix = drive.trim_end_matches(['\\', '/']);

        let default_windows = format!(r"{drive_prefix}\Windows");
        let default_pf = format!(r"{drive_prefix}\Program Files");
        let default_pf86 = format!(r"{drive_prefix}\Program Files (x86)");
        let default_program_data = format!(r"{drive_prefix}\ProgramData");
        let default_users = format!(r"{drive_prefix}\Users");

        let mut subtree_roots = Vec::new();
        for raw in [
            system_root.unwrap_or(&default_windows),
            program_files.unwrap_or(&default_pf),
            program_files_x86.unwrap_or(&default_pf86),
        ]
        .into_iter()
        .chain(program_w6432)
        {
            push_unique_components(&mut subtree_roots, raw);
        }

        let mut exact_only_roots = Vec::new();
        push_unique_components(
            &mut exact_only_roots,
            program_data.unwrap_or(&default_program_data),
        );
        push_unique_components(&mut exact_only_roots, &default_users);
        if let Some(profile) = user_profile.map(str::trim).filter(|s| !s.is_empty())
            && let Some(users_dir) = Path::new(profile).parent()
            && users_dir.parent().is_some()
        {
            push_unique_components(&mut exact_only_roots, &users_dir.to_string_lossy());
        }

        Self {
            subtree_roots,
            exact_only_roots,
        }
    }
}

fn extract_drive_prefix(path: &str) -> Option<&str> {
    let trimmed = path
        .trim()
        .strip_prefix(r"\\?\")
        .or_else(|| path.trim().strip_prefix("//?/"))
        .unwrap_or(path.trim());
    let bytes = trimmed.as_bytes();
    if bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' {
        Some(&trimmed[..2])
    } else {
        None
    }
}

fn canonical_components_lower(path: &Path) -> Vec<String> {
    let raw = path.to_string_lossy();
    let stripped = raw
        .strip_prefix(r"\\?\")
        .or_else(|| raw.strip_prefix("//?/"))
        .unwrap_or(&raw);
    stripped
        .replace('/', "\\")
        .split('\\')
        .filter(|s| !s.is_empty())
        .map(|s| s.to_ascii_lowercase())
        .collect()
}

fn push_unique_components(target: &mut Vec<Vec<String>>, raw: &str) {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return;
    }
    let resolved = dunce::canonicalize(trimmed).unwrap_or_else(|_| PathBuf::from(trimmed));
    let parts = canonical_components_lower(&resolved);
    if !parts.is_empty() && !target.contains(&parts) {
        target.push(parts);
    }
}

fn is_protected_system_path_with_roots(canonical: &Path, roots: &ProtectedRoots) -> bool {
    // Reject drive root (e.g. `C:\`, `D:\`)
    if canonical.parent().is_none() {
        return true;
    }

    let parts = canonical_components_lower(canonical);
    if parts.len() <= 1 {
        return true;
    }

    if roots
        .subtree_roots
        .iter()
        .any(|root| parts.len() >= root.len() && parts[..root.len()] == root[..])
    {
        return true;
    }

    roots.exact_only_roots.contains(&parts)
}

fn resolve_existing_or_parent(path: &Path) -> AppResult<PathBuf> {
    let mut current = path.to_path_buf();
    let mut missing_suffix = Vec::new();

    let canonical_existing = loop {
        match dunce::canonicalize(&current) {
            Ok(canonical) => break canonical,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                match std::fs::symlink_metadata(&current) {
                    Ok(_) => {
                        return Err(AppError::InvalidArgument(format!(
                            "refusing to traverse unresolved reparse point: {}",
                            current.display()
                        )));
                    }
                    Err(meta_err) if meta_err.kind() == std::io::ErrorKind::NotFound => {}
                    Err(meta_err) => return Err(AppError::Io(meta_err)),
                }

                let name = current.file_name().ok_or_else(|| {
                    AppError::InvalidArgument(format!(
                        "destination has no existing parent: {}",
                        path.display()
                    ))
                })?;
                missing_suffix.push(name.to_os_string());
                current = current.parent().map(Path::to_path_buf).ok_or_else(|| {
                    AppError::InvalidArgument(format!(
                        "destination has no existing parent: {}",
                        path.display()
                    ))
                })?;
            }
            Err(err) => {
                return Err(AppError::InvalidArgument(format!(
                    "destination canonicalize failed: {err} ({})",
                    current.display()
                )));
            }
        }
    };

    let mut resolved = canonical_existing;
    for part in missing_suffix.iter().rev() {
        resolved.push(part);
    }
    Ok(resolved)
}

/// Returns true when `dir` is a git repository, worktree, or resides inside one
/// (checks `dir` and all existing ancestor directories for a `.git` folder or file).
pub fn is_git_repository(dir: &Path) -> bool {
    let start = dunce::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
    start.ancestors().any(|anc| anc.join(".git").exists())
}

/// Returns true when two normalized paths are identical or one is a parent/descendant of the other
/// (case-insensitive on Windows).
pub fn paths_overlap(a: &Path, b: &Path) -> bool {
    let parts = |p: &Path| -> Vec<String> {
        p.to_string_lossy()
            .replace('/', "\\")
            .trim_end_matches('\\')
            .split('\\')
            .filter(|s| !s.is_empty())
            .map(|s| s.to_ascii_lowercase())
            .collect()
    };
    let pa = parts(a);
    let pb = parts(b);
    if pa.is_empty() || pb.is_empty() {
        return false;
    }
    let min_len = pa.len().min(pb.len());
    pa[..min_len] == pb[..min_len]
}

/// Searches `dir` (directly and inside nested Unity projects up to 3 levels deep) for
/// `Temp\UnityLockfile`, which indicates the Unity editor currently has the project open.
pub fn find_unity_lockfile(dir: &Path) -> Option<PathBuf> {
    let direct = dir.join("Temp").join("UnityLockfile");
    if direct.exists() {
        return Some(direct);
    }
    find_unity_lockfile_recursive(dir, 3)
}

fn find_unity_lockfile_recursive(dir: &Path, depth: usize) -> Option<PathBuf> {
    if dir
        .join("ProjectSettings")
        .join("ProjectVersion.txt")
        .is_file()
    {
        let lock = dir.join("Temp").join("UnityLockfile");
        return lock.exists().then_some(lock);
    }
    let next_depth = depth.checked_sub(1)?;
    let entries = std::fs::read_dir(dir).ok()?;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let plain_dir = entry.file_type().is_ok_and(|t| t.is_dir());
        if plain_dir
            && !name.starts_with('.')
            && name != "node_modules"
            && let Some(found) = find_unity_lockfile_recursive(&entry.path(), next_depth)
        {
            return Some(found);
        }
    }
    None
}

/// Converts a snapshot source path (e.g. `D:\Projects\ProjectAlpha`, `\\?\D:\Projects\ProjectAlpha`,
/// or `/D/Projects/ProjectAlpha`) into Restic's `<snapshotId>:<subpath>` POSIX form
/// (`/D/Projects/ProjectAlpha`). Rejects empty paths, drive roots, and `..` traversal.
pub fn windows_path_to_restic_subpath(raw: &str) -> AppResult<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(AppError::InvalidArgument(
            "snapshot source path must not be empty".to_owned(),
        ));
    }
    let stripped = trimmed
        .strip_prefix(r"\\?\")
        .or_else(|| trimmed.strip_prefix("//?/"))
        .unwrap_or(trimmed);

    let normalized = stripped.replace('\\', "/");
    let parts: Vec<&str> = normalized.split('/').filter(|s| !s.is_empty()).collect();
    if parts.is_empty() {
        return Err(AppError::InvalidArgument(format!(
            "snapshot source path is invalid: {raw}"
        )));
    }
    if parts.contains(&"..") || parts.contains(&".") {
        return Err(AppError::InvalidArgument(format!(
            "snapshot source path must not contain '.' or '..': {raw}"
        )));
    }

    let first = parts[0];
    let mut out = Vec::with_capacity(parts.len());
    if first.len() == 2 && first.as_bytes()[1] == b':' && first.as_bytes()[0].is_ascii_alphabetic()
    {
        if parts.len() < 2 {
            return Err(AppError::InvalidArgument(format!(
                "refusing to use bare drive root as snapshot subpath: {raw}"
            )));
        }
        out.push(first[..1].to_owned());
        out.extend(parts[1..].iter().map(|s| (*s).to_owned()));
    } else if normalized.starts_with('/') {
        if parts.len() < 2 && first.len() == 1 && first.as_bytes()[0].is_ascii_alphabetic() {
            return Err(AppError::InvalidArgument(format!(
                "refusing to use bare drive root as snapshot subpath: {raw}"
            )));
        }
        out.extend(parts.iter().map(|s| (*s).to_owned()));
    } else {
        return Err(AppError::InvalidArgument(format!(
            "snapshot source path must be absolute: {raw}"
        )));
    }

    Ok(format!("/{}", out.join("/")))
}

/// Extracts the final folder name from a Windows or POSIX snapshot path.
pub fn snapshot_path_folder_name(raw: &str) -> Option<&str> {
    raw.trim()
        .trim_end_matches(['\\', '/'])
        .rsplit(['\\', '/'])
        .next()
        .filter(|s| !s.is_empty() && !(s.len() == 2 && s.as_bytes()[1] == b':'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonicalize_existing_dir_accepts_real_directory() {
        let dir = tempfile::tempdir().expect("tempdir");
        let resolved = canonicalize_existing_dir(dir.path(), "backup target").expect("valid dir");
        assert!(resolved.is_dir());
    }

    #[test]
    fn canonicalize_existing_dir_rejects_file_and_parent_traversal() {
        let dir = tempfile::tempdir().expect("tempdir");
        let file_path = dir.path().join("sample.txt");
        std::fs::write(&file_path, b"hello").expect("write");

        assert!(canonicalize_existing_dir(&file_path, "backup target").is_err());
        let traversal = dir.path().join("..").join("escaped");
        assert!(canonicalize_existing_dir(&traversal, "backup target").is_err());
    }

    #[test]
    fn validate_restore_destination_allows_new_subfolder_inside_tempdir() {
        let dir = tempfile::tempdir().expect("tempdir");
        let target = dir.path().join("restore-out").join("2026-11-07");
        let resolved = validate_restore_destination(&target).expect("valid restore destination");
        assert!(resolved.ends_with("2026-11-07"));
    }

    #[test]
    fn validate_restore_destination_rejects_drive_root_and_windows_dir() {
        assert!(validate_restore_destination(Path::new(r"C:\")).is_err());
        assert!(validate_restore_destination(Path::new(r"C:\Windows")).is_err());
        assert!(validate_restore_destination(Path::new(r"C:\Windows\System32")).is_err());
    }

    #[test]
    fn protected_system_path_supports_non_c_windows_layout_and_permits_siblings() {
        let non_c_roots = ProtectedRoots::from_values(
            Some("D:"),
            Some(r"D:\Windows"),
            Some(r"D:\Program Files"),
            Some(r"D:\Program Files (x86)"),
            None,
            Some(r"D:\ProgramData"),
            Some(r"D:\Users\alice"),
        );

        for forbidden in [
            r"D:\",
            r"D:\Windows",
            r"D:\WINDOWS\System32",
            r"D:\Program Files",
            r"D:\Program Files\Restic",
            r"D:\Program Files (x86)",
            r"D:\Program Files (x86)\Tool",
            r"D:\ProgramData",
            r"D:\Users",
        ] {
            assert!(
                is_protected_system_path_with_roots(Path::new(forbidden), &non_c_roots),
                "expected {forbidden} to be rejected under D: Windows layout"
            );
        }

        for permitted in [
            r"D:\WindowsBackup",
            r"D:\WindowsBackup\2026",
            r"D:\Program Files Custom\App",
            r"D:\Users\alice\Restores\snap1",
            r"E:\Projects\Restore",
        ] {
            assert!(
                !is_protected_system_path_with_roots(Path::new(permitted), &non_c_roots),
                "expected sibling/user path {permitted} to be allowed"
            );
        }

        // End-to-end check with real filesystem canonicalization and a permitted sibling.
        let dir = tempfile::tempdir().expect("tempdir");
        let fake_win = dir.path().join("Windows");
        let fake_win_sub = fake_win.join("System32");
        let sibling = dir.path().join("WindowsBackup");
        std::fs::create_dir_all(&fake_win_sub).unwrap();
        let fake_roots = ProtectedRoots::from_values(
            None,
            Some(&fake_win.to_string_lossy()),
            Some(&dir.path().join("Program Files").to_string_lossy()),
            Some(&dir.path().join("Program Files (x86)").to_string_lossy()),
            None,
            Some(&dir.path().join("ProgramData").to_string_lossy()),
            None,
        );
        assert!(validate_restore_destination_with_roots(&fake_win, &fake_roots).is_err());
        assert!(validate_restore_destination_with_roots(&fake_win_sub, &fake_roots).is_err());
        assert!(validate_restore_destination_with_roots(&sibling, &fake_roots).is_ok());
    }

    #[test]
    fn is_git_repository_detects_dir_worktree_and_ancestors() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(!is_git_repository(dir.path()));

        std::fs::create_dir(dir.path().join(".git")).unwrap();
        assert!(is_git_repository(dir.path()));

        let nested = dir.path().join("packages").join("unity-app");
        std::fs::create_dir_all(&nested).unwrap();
        assert!(is_git_repository(&nested));

        let worktree = tempfile::tempdir().expect("worktree");
        std::fs::write(worktree.path().join(".git"), "gitdir: /somewhere").unwrap();
        assert!(is_git_repository(worktree.path()));
    }

    #[test]
    fn paths_overlap_detects_same_parent_and_child_only() {
        assert!(paths_overlap(
            Path::new(r"D:\Projects\ProjectAlpha"),
            Path::new(r"d:\projects\projectalpha\")
        ));
        assert!(paths_overlap(
            Path::new(r"D:\Projects"),
            Path::new(r"D:\Projects\ProjectAlpha")
        ));
        assert!(paths_overlap(
            Path::new(r"D:\Projects\ProjectAlpha\Assets"),
            Path::new(r"D:\Projects\ProjectAlpha")
        ));
        assert!(!paths_overlap(
            Path::new(r"D:\Projects\ProjectAlpha"),
            Path::new(r"D:\Projects\ProjectAlpha2")
        ));
        assert!(!paths_overlap(
            Path::new(r"D:\Projects\A"),
            Path::new(r"E:\Projects\A")
        ));
    }

    #[test]
    fn find_unity_lockfile_detects_direct_and_nested_projects() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(find_unity_lockfile(dir.path()).is_none());

        std::fs::create_dir_all(dir.path().join(r"nested\Game\ProjectSettings")).unwrap();
        std::fs::write(
            dir.path()
                .join(r"nested\Game\ProjectSettings\ProjectVersion.txt"),
            "m_EditorVersion: 2022.3.22f1",
        )
        .unwrap();
        std::fs::create_dir_all(dir.path().join(r"nested\Game\Temp")).unwrap();
        std::fs::write(dir.path().join(r"nested\Game\Temp\UnityLockfile"), b"").unwrap();
        assert!(find_unity_lockfile(dir.path()).is_some());

        let direct = tempfile::tempdir().expect("direct");
        std::fs::create_dir_all(direct.path().join("Temp")).unwrap();
        std::fs::write(direct.path().join("Temp").join("UnityLockfile"), b"").unwrap();
        assert!(find_unity_lockfile(direct.path()).is_some());
    }

    #[test]
    fn windows_path_to_restic_subpath_converts_and_validates() {
        assert_eq!(
            windows_path_to_restic_subpath(r"D:\Projects\ProjectAlpha").unwrap(),
            "/D/Projects/ProjectAlpha"
        );
        assert_eq!(
            windows_path_to_restic_subpath(r"\\?\E:\Projects\ProjectAlpha\").unwrap(),
            "/E/Projects/ProjectAlpha"
        );
        assert_eq!(
            windows_path_to_restic_subpath("/D/Projects/ProjectAlpha").unwrap(),
            "/D/Projects/ProjectAlpha"
        );
        assert!(windows_path_to_restic_subpath(r"D:\").is_err());
        assert!(windows_path_to_restic_subpath("/D").is_err());
        assert!(windows_path_to_restic_subpath(r"D:\a\..\b").is_err());
        assert!(windows_path_to_restic_subpath("relative/path").is_err());
        assert_eq!(
            snapshot_path_folder_name(r"D:\Projects\ProjectAlpha\"),
            Some("ProjectAlpha")
        );
        assert_eq!(
            snapshot_path_folder_name("/E/Projects/ProjectAlpha"),
            Some("ProjectAlpha")
        );
    }
}
