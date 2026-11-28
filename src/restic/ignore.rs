//! Gitignore-style ignore sources for backup and restore: each backup target's root `.gitignore`
//! (opt-in via `useGitignore`) and custom ignore files in gitignore syntax (`ignoreFiles`,
//! resolved relative to each target or absolute).
//!
//! Patterns are converted to Restic exclude patterns, whose syntax is close to gitignore:
//! unanchored patterns match complete path components at any depth, `**` crosses directories,
//! a leading `/` anchors at the backup root, and `!` re-includes a previously matched path.
//! Nested `.gitignore` files are not read; only each target's root `.gitignore` is honored.

use std::path::{Path, PathBuf};

use crate::config::{AppConfig, BackupTarget};
use crate::error::{AppError, AppResult};

/// Patterns (already anchored to `anchor` when the source lives inside a target) collected from
/// all ignore sources of one backup target. Order is preserved because `!` negations only cancel
/// patterns listed before them.
pub(crate) fn target_ignore_patterns(
    config: &AppConfig,
    target: &BackupTarget,
) -> AppResult<Vec<String>> {
    dir_ignore_patterns(config, Path::new(&target.path))
}

/// Same as [`target_ignore_patterns`] for an arbitrary directory (e.g. a restore destination).
pub(crate) fn dir_ignore_patterns(config: &AppConfig, target_dir: &Path) -> AppResult<Vec<String>> {
    let mut patterns = Vec::new();
    if config.use_gitignore {
        let gitignore = target_dir.join(".gitignore");
        if gitignore.is_file() {
            let anchor = posix_prefix(target_dir);
            read_ignore_file(&gitignore, Some(&anchor), &mut patterns)?;
        }
    }
    for entry in &config.ignore_files {
        let path = resolve_ignore_file(entry, target_dir)?;
        if path.is_file() {
            let anchor = path
                .parent()
                .filter(|p| p.starts_with(target_dir))
                .map(posix_prefix);
            read_ignore_file(&path, anchor.as_deref(), &mut patterns)?;
        }
    }
    Ok(patterns)
}

/// Same sources relative to a restore destination folder, for
/// `restic restore --exclude` parity with what `restic backup` left out.
pub(crate) fn restore_ignore_patterns(
    config: &AppConfig,
    local_dir: &Path,
) -> AppResult<Vec<String>> {
    dir_ignore_patterns(config, local_dir)
}

fn posix_prefix(path: &Path) -> String {
    path.to_string_lossy()
        .replace('\\', "/")
        .trim_matches('/')
        .to_owned()
}

fn resolve_ignore_file(entry: &str, target_dir: &Path) -> AppResult<PathBuf> {
    let trimmed = entry.trim();
    let candidate = Path::new(trimmed);
    if candidate.is_absolute() {
        Ok(candidate.to_path_buf())
    } else {
        Ok(target_dir.join(trimmed))
    }
}

fn read_ignore_file(path: &Path, anchor: Option<&str>, out: &mut Vec<String>) -> AppResult<()> {
    let raw = std::fs::read_to_string(path).map_err(|e| {
        AppError::InvalidArgument(format!(
            "ignore ファイル {} の読み込みに失敗したため、バックアップ/復元を中止しました: {e}",
            path.display()
        ))
    })?;
    for line in raw.lines() {
        if let Some(pattern) = convert_gitignore_pattern(line, anchor)
            && !out.contains(&pattern)
        {
            out.push(pattern);
        }
    }
    Ok(())
}

/// Converts one gitignore line into a Restic exclude pattern:
/// - blank lines and `#` comments are dropped;
/// - `!` negations keep their gitignore meaning (Restic cancels earlier matches);
/// - a leading `/` anchors at the ignore file's directory (`anchor`) when known, and is dropped
///   otherwise (unanchored patterns match at any depth);
/// - trailing `/` is preserved verbatim (Restic ignores it, matching files named like the
///   directory too, which gitignore would not).
pub(crate) fn convert_gitignore_pattern(line: &str, anchor: Option<&str>) -> Option<String> {
    let trimmed = line.trim_end();
    if trimmed.is_empty() || trimmed.starts_with('#') {
        return None;
    }
    let (negated, body) = if let Some(rest) = trimmed.strip_prefix('!') {
        (true, rest)
    } else {
        (false, trimmed)
    };
    let body = body.trim_end();
    if body.is_empty() {
        return None;
    }
    let converted = if let Some(rest) = body.strip_prefix('/') {
        match anchor {
            Some(a) if !a.is_empty() => format!("{a}/{rest}"),
            _ => rest.to_owned(),
        }
    } else {
        body.to_owned()
    };
    if negated {
        Some(format!("!{converted}"))
    } else {
        Some(converted)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conversion_skips_comments_and_blanks() {
        assert_eq!(convert_gitignore_pattern("# comment", None), None);
        assert_eq!(convert_gitignore_pattern("", None), None);
        assert_eq!(convert_gitignore_pattern("   ", None), None);
    }

    #[test]
    fn conversion_passes_unanchored_patterns_through() {
        assert_eq!(
            convert_gitignore_pattern("build/", None).as_deref(),
            Some("build/")
        );
        assert_eq!(
            convert_gitignore_pattern("*.log", None).as_deref(),
            Some("*.log")
        );
        assert_eq!(
            convert_gitignore_pattern("a/**/b", None).as_deref(),
            Some("a/**/b")
        );
    }

    #[test]
    fn conversion_anchors_leading_slash_to_known_dir() {
        assert_eq!(
            convert_gitignore_pattern("/build", Some("D:/Projects/World")).as_deref(),
            Some("D:/Projects/World/build")
        );
        assert_eq!(
            convert_gitignore_pattern("/logs/*.txt", Some("D:/Projects/World")).as_deref(),
            Some("D:/Projects/World/logs/*.txt")
        );
        // Without an anchor the pattern stays unanchored rather than filesystem-rooted.
        assert_eq!(
            convert_gitignore_pattern("/build", None).as_deref(),
            Some("build")
        );
    }

    #[test]
    fn conversion_keeps_negations_with_anchor() {
        assert_eq!(
            convert_gitignore_pattern("!keep.bin", Some("D:/T")).as_deref(),
            Some("!D:/T/keep.bin")
        );
        assert_eq!(
            convert_gitignore_pattern("!/keep.bin", Some("D:/T")).as_deref(),
            Some("!D:/T/keep.bin")
        );
    }

    #[test]
    fn conversion_drops_bare_negation() {
        assert_eq!(convert_gitignore_pattern("!", Some("D:/T")), None);
    }

    #[test]
    fn target_ignore_patterns_reads_root_gitignore_when_enabled() {
        let dir = tempfile::tempdir().unwrap();
        let target_path = dir.path().join("proj");
        std::fs::create_dir_all(&target_path).unwrap();
        std::fs::write(
            target_path.join(".gitignore"),
            "# c\n/build\n*.log\n!keep.log\n",
        )
        .unwrap();

        let mut config = AppConfig::default();
        let disabled = dir_ignore_patterns(&config, &target_path).unwrap();
        assert!(disabled.is_empty());

        config.use_gitignore = true;
        let patterns = dir_ignore_patterns(&config, &target_path).unwrap();
        let anchor = target_path.to_string_lossy().replace('\\', "/");
        assert_eq!(
            patterns,
            vec![
                format!("{anchor}/build"),
                "*.log".to_owned(),
                format!("!{anchor}/keep.log"),
            ]
        );
    }

    #[test]
    fn target_ignore_patterns_reads_custom_ignore_files_relative_to_target() {
        let dir = tempfile::tempdir().unwrap();
        let target_path = dir.path().join("proj");
        std::fs::create_dir_all(target_path.join("sub")).unwrap();
        std::fs::write(target_path.join(".resticignore"), "vendor/\n").unwrap();
        std::fs::write(target_path.join("sub/.gitignore"), "/out\n").unwrap();

        let config = AppConfig {
            ignore_files: vec![".resticignore".to_owned(), "sub/.gitignore".to_owned()],
            ..Default::default()
        };
        let patterns = dir_ignore_patterns(&config, &target_path).unwrap();
        assert_eq!(patterns.len(), 2);
        // Unanchored custom pattern and an anchored one relative to its own directory.
        assert_eq!(patterns[0], "vendor/");
        let sub = target_path.join("sub").to_string_lossy().replace('\\', "/");
        assert_eq!(patterns[1], format!("{sub}/out"));
    }

    #[test]
    fn missing_ignore_sources_are_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let config = AppConfig {
            use_gitignore: true,
            ignore_files: vec![".does-not-exist".to_owned()],
            ..Default::default()
        };
        assert!(dir_ignore_patterns(&config, dir.path()).unwrap().is_empty());
    }
}
