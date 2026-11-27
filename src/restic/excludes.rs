//! Which paths a backup leaves out: the `--exclude` arguments, Unity project folders found on
//! disk, the guard against a pattern that would drop a whole target, and the settings hash.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use crate::config::{AppConfig, BASIC_EXCLUDES, BASIC_UNITY_PROJECT_EXCLUDES, BackupTarget};
use crate::error::{AppError, AppResult};

/// `--exclude-if-present` and `--exclude` arguments for `restic backup`. Fails instead of
/// passing a pattern that matches a target's own path, which would leave the whole folder out.
pub(super) fn backup_filter_args(
    config: &AppConfig,
    targets: &[BackupTarget],
) -> AppResult<Vec<String>> {
    let markers = config.exclude_markers();
    // restic keeps the marker file itself (Unity's `Library/SourceAssetDB` can be 1 GB), so
    // exclude it by name too. A `filename:header` marker names the file before the colon.
    let marker_files = markers
        .iter()
        .filter_map(|m| m.split(':').next())
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let target_patterns = targets
        .iter()
        .flat_map(|t| t.exclude_patterns.iter().map(|p| p.trim()))
        .filter(|p| !p.is_empty());
    let unity_excludes: Vec<String> = if config.use_basic_excludes {
        targets
            .iter()
            .flat_map(|t| unity_project_excludes(Path::new(&t.path)))
            .collect()
    } else {
        Vec::new()
    };
    let excludes: Vec<&str> = config
        .shared_excludes()
        .into_iter()
        .chain(unity_excludes.iter().map(String::as_str))
        .chain(marker_files)
        .chain(target_patterns)
        .collect();

    for target in targets {
        if let Some(pattern) = excludes.iter().find(|p| covers_path(p, &target.path)) {
            let hint = if BASIC_EXCLUDES.contains(pattern) {
                "基本の除外パターンに含まれるため、フォルダ画面で基本の除外パターンをオフにするか、対象フォルダを別の場所へ移動してください。"
            } else {
                "設定した除外パターンを見直してください。"
            };
            return Err(AppError::InvalidArgument(format!(
                "除外パターン「{pattern}」がバックアップ対象「{}」のパスそのものに一致し、フォルダ全体が除外されるため、バックアップを中止しました。{hint}",
                target.path
            )));
        }
    }

    let mut args = Vec::new();
    for marker in markers {
        args.extend(["--exclude-if-present".to_owned(), marker.to_owned()]);
    }
    for pattern in excludes {
        args.extend(["--exclude".to_owned(), pattern.to_owned()]);
    }
    Ok(args)
}

/// 64-bit FNV-1a of the exclude settings (not of folders found on disk, such as a Unity `Temp`
/// that exists only while the editor is open), stable across builds unlike `DefaultHasher`.
pub(crate) fn exclude_fingerprint(config: &AppConfig, targets: &[BackupTarget]) -> String {
    let unity: &[&str] = if config.use_basic_excludes {
        BASIC_UNITY_PROJECT_EXCLUDES
    } else {
        &[]
    };
    let hash = config
        .shared_excludes()
        .into_iter()
        .chain(config.exclude_markers())
        .chain(unity.iter().copied())
        .chain(
            targets
                .iter()
                .flat_map(|t| t.exclude_patterns.iter().map(String::as_str)),
        )
        .flat_map(|setting| setting.bytes().chain([0]))
        .fold(0xcbf2_9ce4_8422_2325_u64, |h, b| {
            (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
        });
    format!("{hash:016x}")
}

/// Escapes Restic glob metacharacters (`[`, `*`, `?`) using bracket character classes so an
/// actual on-disk relative folder path is matched literally during `restore --exclude` on Windows.
///
/// NOTE: Backslash escaping (`\]`, `\[`) MUST NOT be used on Windows because Restic normalizes
/// `\` to `/` in exclude patterns before matching (`internal/filter/filter.go`). Bare `]` outside
/// a character class is already literal in Restic / Go `path.Match`.
fn escape_restic_glob_literal(rel_posix: &str) -> String {
    let mut out = String::with_capacity(rel_posix.len());
    for ch in rel_posix.chars() {
        match ch {
            '[' => out.push_str("[[]"),
            '*' => out.push_str("[*]"),
            '?' => out.push_str("[?]"),
            _ => out.push(ch),
        }
    }
    out
}

/// Case-sensitive single path-segment glob matcher matching Go `path.Match` / Restic's
/// `internal/filter/filter.go` (`*`, `?`, and `[...]` character classes).
fn restic_segment_matches(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    fn rec(p: &[char], t: &[char]) -> bool {
        if p.is_empty() {
            return t.is_empty();
        }
        match p[0] {
            '*' => {
                // Skip consecutive stars.
                let mut next_p = 1;
                while next_p < p.len() && p[next_p] == '*' {
                    next_p += 1;
                }
                for skip in 0..=t.len() {
                    if rec(&p[next_p..], &t[skip..]) {
                        return true;
                    }
                }
                false
            }
            '?' => !t.is_empty() && rec(&p[1..], &t[1..]),
            '[' => {
                if t.is_empty() {
                    return false;
                }
                let mut i = 1;
                let negated = i < p.len() && (p[i] == '^' || p[i] == '!');
                if negated {
                    i += 1;
                }
                let mut matched = false;
                let mut closed = false;
                let mut first = true;
                while i < p.len() {
                    if p[i] == ']' && !first {
                        closed = true;
                        i += 1;
                        break;
                    }
                    first = false;
                    let lo = p[i];
                    if i + 2 < p.len() && p[i + 1] == '-' && p[i + 2] != ']' {
                        let hi = p[i + 2];
                        if t[0] >= lo && t[0] <= hi {
                            matched = true;
                        }
                        i += 3;
                    } else {
                        if t[0] == lo {
                            matched = true;
                        }
                        i += 1;
                    }
                }
                if !closed {
                    // Unclosed `[` does not match in Go `path.Match`.
                    return false;
                }
                let ok = if negated { !matched } else { matched };
                ok && rec(&p[i..], &t[1..])
            }
            ch => !t.is_empty() && t[0] == ch && rec(&p[1..], &t[1..]),
        }
    }
    rec(&p, &t)
}

/// Converts a shared or target-specific exclude pattern into pattern(s) suitable for
/// `restic restore <id>:<subpath> --target <local_dir>` rooted at `local_norm` (POSIX forward-slash
/// normalized).
///
/// Matches Restic 0.17.3's **case-sensitive** segment rules (`internal/filter/filter.go#L175-L195`):
/// - Strips an absolute `local_norm/` prefix safely on UTF-8 boundaries.
/// - For relative multi-segment patterns whose leading components match a trailing path suffix of
///   `local_norm` using case-sensitive glob matching (e.g. target `D:/Projects/World` with
///   exclude `Wor*/local-settings.json` -> `/local-settings.json`, while `world/scene.txt` does NOT
///   falsely match `World`!), emits the root-anchored remainder so subpath `restic restore`
///   protects the exact same files that `restic backup` excluded.
fn normalize_exclude_for_restore_root(pat: &str, local_norm: &str) -> Vec<String> {
    let norm = pat.trim().replace('\\', "/");
    if norm.is_empty() {
        return Vec::new();
    }
    if norm.len() > local_norm.len()
        && norm
            .get(..local_norm.len())
            .is_some_and(|prefix| prefix == local_norm)
        && norm.as_bytes().get(local_norm.len()) == Some(&b'/')
    {
        return vec![norm[local_norm.len()..].to_owned()];
    }

    let mut out = vec![norm.clone()];
    if !norm.starts_with('/') && norm.contains('/') {
        let local_parts: Vec<&str> = local_norm.split('/').filter(|s| !s.is_empty()).collect();
        let pat_parts: Vec<&str> = norm.split('/').filter(|s| !s.is_empty()).collect();
        for start in 0..local_parts.len() {
            let suffix = &local_parts[start..];
            let k = suffix.len();
            if k < pat_parts.len()
                && pat_parts[..k]
                    .iter()
                    .zip(suffix.iter())
                    .all(|(p_seg, l_seg)| restic_segment_matches(p_seg, l_seg))
            {
                let anchored = format!("/{}", pat_parts[k..].join("/"));
                if !out.iter().any(|existing| existing == &anchored) {
                    out.push(anchored);
                }
            }
        }
    }
    out
}

/// Builds `--exclude` patterns for `restic restore <id>:<subpath> --target <local_dir>` (used
/// both with `--delete` and without `--delete` so receiver-excluded files are neither deleted nor
/// overwritten by the sender's snapshot).
///
/// Returns `Err` if `local_dir` itself matches an exclude marker, if any subdirectory or marker
/// file cannot be read (failing closed rather than proceeding with an incomplete exclude set), or
/// if `cancel_flag` is set during the directory walk.
pub(crate) fn restore_delete_exclude_patterns(
    config: &AppConfig,
    local_dir: &Path,
    target_excludes: &[String],
    cancel_flag: Option<&AtomicBool>,
) -> AppResult<Vec<String>> {
    let mut patterns: Vec<String> = Vec::new();
    let mut push_unique = |p: String| {
        let trimmed = p.trim();
        if !trimmed.is_empty() && !patterns.iter().any(|existing| existing == trimmed) {
            patterns.push(trimmed.to_owned());
        }
    };

    let local_norm = local_dir
        .to_string_lossy()
        .replace('\\', "/")
        .trim_end_matches('/')
        .to_owned();

    for pat in config.shared_excludes() {
        for converted in normalize_exclude_for_restore_root(pat, &local_norm) {
            push_unique(converted);
        }
    }

    let markers = config.exclude_markers();
    for marker in &markers {
        if let Some(name) = marker.split(':').next().map(str::trim)
            && !name.is_empty()
        {
            push_unique(escape_restic_glob_literal(name));
        }
    }
    if local_dir.is_dir() && !markers.is_empty() {
        if dir_matches_exclude_marker(local_dir, &markers)? {
            return Err(AppError::InvalidArgument(format!(
                "復元先フォルダ自体に除外マーカー（excludeIfPresent）が存在するため復元を中止しました: {}",
                local_dir.display()
            )));
        }
        let mut marker_dirs = Vec::new();
        find_marker_dirs(
            local_dir,
            local_dir,
            &markers,
            cancel_flag,
            &mut marker_dirs,
        )?;
        for rel in marker_dirs {
            push_unique(format!("/{}", escape_restic_glob_literal(&rel)));
        }
    }

    if config.use_basic_excludes {
        let mut unity_projects = Vec::new();
        if local_dir.is_dir() {
            find_unity_projects(local_dir, UNITY_SCAN_DEPTH, &mut unity_projects);
        }
        // Also protect root Unity generated folders when the destination has Unity structure
        // (e.g. Assets/ or ProjectSettings/ or Library/) even if ProjectVersion.txt is missing yet.
        let looks_like_root_unity = local_dir.join("ProjectSettings").is_dir()
            || local_dir.join("Assets").is_dir()
            || local_dir.join("Library").is_dir();
        if looks_like_root_unity && !unity_projects.iter().any(|p| p == local_dir) {
            unity_projects.push(local_dir.to_path_buf());
        }
        for project in &unity_projects {
            let rel_prefix = project
                .strip_prefix(local_dir)
                .ok()
                .map(|rel| rel.to_string_lossy().replace('\\', "/"))
                .unwrap_or_default();
            let rel_prefix = rel_prefix.trim_matches('/');
            for rel in BASIC_UNITY_PROJECT_EXCLUDES {
                let norm_rel = rel.replace('\\', "/");
                let anchored = if rel_prefix.is_empty() {
                    format!("/{}", escape_restic_glob_literal(&norm_rel))
                } else {
                    format!(
                        "/{}/{}",
                        escape_restic_glob_literal(rel_prefix),
                        escape_restic_glob_literal(&norm_rel)
                    )
                };
                push_unique(anchored);
                if let Some(actual) = existing_child(project, rel)
                    && let Ok(actual_rel) = actual.strip_prefix(local_dir)
                {
                    let actual_norm = actual_rel
                        .to_string_lossy()
                        .replace('\\', "/")
                        .trim_matches('/')
                        .to_owned();
                    if !actual_norm.is_empty() {
                        push_unique(format!("/{}", escape_restic_glob_literal(&actual_norm)));
                    }
                }
            }
        }
    }

    for pat in target_excludes {
        for converted in normalize_exclude_for_restore_root(pat, &local_norm) {
            push_unique(converted);
        }
    }

    Ok(patterns)
}

/// How many folders below a backup target to look for Unity projects, e.g. `repo\unity\Game`.
const UNITY_SCAN_DEPTH: usize = 3;

fn dir_matches_exclude_marker(dir: &Path, markers: &[&str]) -> AppResult<bool> {
    use std::io::{ErrorKind, Read};

    for marker in markers {
        let (filename, expected_header) = match marker.split_once(':') {
            Some((name, header)) => (name.trim(), Some(header)),
            None => (marker.trim(), None),
        };
        if filename.is_empty() {
            continue;
        }
        let candidate = dir.join(filename);
        let meta = match std::fs::symlink_metadata(&candidate) {
            Ok(m) => m,
            Err(err) if err.kind() == ErrorKind::NotFound => continue,
            Err(err) => {
                return Err(AppError::InvalidArgument(format!(
                    "復元先の除外マーカー確認でファイル状態を取得できませんでした（{}）: {err}",
                    candidate.display()
                )));
            }
        };
        if !meta.is_file() {
            continue;
        }
        let Some(header) = expected_header else {
            return Ok(true);
        };
        let mut file = std::fs::File::open(&candidate).map_err(|err| {
            AppError::InvalidArgument(format!(
                "復元先の除外マーカーファイルを開けなかったため復元を中止しました（{}）: {err}",
                candidate.display()
            ))
        })?;
        let mut buf = vec![0u8; header.len()];
        let mut total = 0usize;
        while total < buf.len() {
            match file.read(&mut buf[total..]) {
                Ok(0) => break,
                Ok(n) => total += n,
                Err(err) => {
                    return Err(AppError::InvalidArgument(format!(
                        "復元先の除外マーカーファイルを読み取れなかったため復元を中止しました（{}）: {err}",
                        candidate.display()
                    )));
                }
            }
        }
        if total == header.len() && buf.as_slice() == header.as_bytes() {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Recursively walks all non-symlink subdirectories under `root` to locate `--exclude-if-present`
/// marker directories, returning an error if any directory/marker cannot be read or if cancelled.
fn find_marker_dirs(
    root: &Path,
    dir: &Path,
    markers: &[&str],
    cancel_flag: Option<&AtomicBool>,
    out: &mut Vec<String>,
) -> AppResult<()> {
    if cancel_flag.is_some_and(|c| c.load(Ordering::Relaxed)) {
        return Err(AppError::Restic(
            "takeover cancelled during restore exclude scan".to_owned(),
        ));
    }
    if dir != root && dir_matches_exclude_marker(dir, markers)? {
        if let Ok(rel) = dir.strip_prefix(root) {
            let norm = rel
                .to_string_lossy()
                .replace('\\', "/")
                .trim_matches('/')
                .to_owned();
            if !norm.is_empty() {
                out.push(norm);
            }
        }
        return Ok(());
    }
    let entries = std::fs::read_dir(dir).map_err(|err| {
        AppError::InvalidArgument(format!(
            "復元先の除外マーカー確認でフォルダを読み取れませんでした（{}）: {err}",
            dir.display()
        ))
    })?;
    for entry_res in entries {
        if cancel_flag.is_some_and(|c| c.load(Ordering::Relaxed)) {
            return Err(AppError::Restic(
                "takeover cancelled during restore exclude scan".to_owned(),
            ));
        }
        let entry = entry_res.map_err(|err| {
            AppError::InvalidArgument(format!(
                "復元先の除外マーカー確認でエントリを読み取れませんでした（{}）: {err}",
                dir.display()
            ))
        })?;
        let ft = entry.file_type().map_err(|err| {
            AppError::InvalidArgument(format!(
                "復元先の除外マーカー確認でファイル種別を取得できませんでした（{}）: {err}",
                entry.path().display()
            ))
        })?;
        if ft.is_dir() && !ft.is_symlink() {
            find_marker_dirs(root, &entry.path(), markers, cancel_flag, out)?;
        }
    }
    Ok(())
}

/// Absolute paths of `BASIC_UNITY_PROJECT_EXCLUDES` that exist in each Unity project found in
/// `target`, spelled as on disk because restic's `--exclude` is case-sensitive.
/// shortcut: a `[` in a project path reads as a restic pattern class and the folder is kept;
/// escape it if such paths show up.
fn unity_project_excludes(target: &Path) -> Vec<String> {
    let mut projects = Vec::new();
    find_unity_projects(target, UNITY_SCAN_DEPTH, &mut projects);
    projects
        .iter()
        .flat_map(|project| {
            BASIC_UNITY_PROJECT_EXCLUDES
                .iter()
                .filter_map(move |rel| existing_child(project, rel))
        })
        .map(|path| path.to_string_lossy().into_owned())
        .collect()
}

/// Unity projects at `dir` or up to `depth` folders below it, not looking inside a project.
/// Skips dot folders, `node_modules`, links and junctions, and folders it cannot read.
fn find_unity_projects(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) {
    if dir
        .join("ProjectSettings")
        .join("ProjectVersion.txt")
        .is_file()
    {
        out.push(dir.to_path_buf());
        return;
    }
    let Some(depth) = depth.checked_sub(1) else {
        return;
    };
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let plain_dir = entry.file_type().is_ok_and(|t| t.is_dir());
        if plain_dir && !name.starts_with('.') && name != "node_modules" {
            find_unity_projects(&entry.path(), depth, out);
        }
    }
}

/// `base` joined with backslash-separated `rel`, each part matched case-insensitively to a
/// folder that exists, in its on-disk spelling.
fn existing_child(base: &Path, rel: &str) -> Option<PathBuf> {
    rel.split('\\').try_fold(base.to_path_buf(), |dir, part| {
        std::fs::read_dir(&dir).ok()?.flatten().find_map(|entry| {
            let is_dir = entry.file_type().is_ok_and(|t| t.is_dir());
            (is_dir
                && entry
                    .file_name()
                    .to_string_lossy()
                    .eq_ignore_ascii_case(part))
            .then(|| entry.path())
        })
    })
}

/// Whether restic would match `pattern` against `path` itself or one of its parent folders.
/// Like restic, a pattern without a leading separator or drive matches at any depth.
fn covers_path(pattern: &str, path: &str) -> bool {
    let parts = |s: &str| -> Vec<String> {
        s.split(['\\', '/'])
            .filter(|c| !c.is_empty())
            .map(str::to_owned)
            .collect()
    };
    let mut pattern_parts = parts(pattern);
    let absolute = pattern.starts_with(['\\', '/']) || pattern.as_bytes().get(1) == Some(&b':');
    if !absolute {
        pattern_parts.insert(0, "**".to_owned());
    }
    covers(&pattern_parts, &parts(path))
}

/// `pattern` matches the leading components of `path`; the rest of `path` lies inside the match.
fn covers(pattern: &[String], path: &[String]) -> bool {
    match pattern.split_first() {
        None => true,
        Some((head, rest)) if head == "**" => (0..=path.len()).any(|i| covers(rest, &path[i..])),
        Some((head, rest)) => path
            .split_first()
            .is_some_and(|(name, tail)| glob_match(head, name) && covers(rest, tail)),
    }
}

/// Go's `filepath.Match` for one path component, which restic uses: `*`, `?` and `[...]`
/// classes with ranges, negated by a leading `^`. Case-sensitive, as restic is.
fn glob_match(pattern: &str, name: &str) -> bool {
    let pattern: Vec<char> = pattern.chars().collect();
    let name: Vec<char> = name.chars().collect();
    glob_chars(&pattern, &name)
}

fn glob_chars(p: &[char], n: &[char]) -> bool {
    match (p, n) {
        ([], _) => n.is_empty(),
        (['*', rest @ ..], _) => (0..=n.len()).any(|i| glob_chars(rest, &n[i..])),
        (['?', rest @ ..], [_, tail @ ..]) => glob_chars(rest, tail),
        (['[', class @ ..], [c, tail @ ..]) if class.len() > 1 => {
            let Some(close) = class.iter().skip(1).position(|&x| x == ']').map(|i| i + 1) else {
                return *c == '[' && glob_chars(class, tail);
            };
            let (negate, set) = match class[0] {
                '^' => (true, &class[1..close]),
                _ => (false, &class[..close]),
            };
            in_class(set, *c) != negate && glob_chars(&class[close + 1..], tail)
        }
        ([x, rest @ ..], [c, tail @ ..]) => x == c && glob_chars(rest, tail),
        _ => false,
    }
}

fn in_class(set: &[char], c: char) -> bool {
    match set {
        [] => false,
        [lo, '-', hi, rest @ ..] => (*lo..=*hi).contains(&c) || in_class(rest, c),
        [x, rest @ ..] => *x == c || in_class(rest, c),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(path: &str) -> BackupTarget {
        BackupTarget {
            id: path.to_owned(),
            label: path.to_owned(),
            path: path.to_owned(),
            enabled: true,
            project_key: None,
            writer_state: crate::config::WriterState::Active,
            exclude_patterns: Vec::new(),
            tags: Vec::new(),
        }
    }

    fn pairs(args: &[String], flag: &str) -> Vec<String> {
        args.chunks(2)
            .filter(|c| c[0] == flag)
            .map(|c| c[1].clone())
            .collect()
    }

    #[test]
    fn backup_filter_args_also_excludes_each_marker_file_by_name() {
        let args = backup_filter_args(&AppConfig::default(), &[target(r"D:\p")]).unwrap();
        let markers = pairs(&args, "--exclude-if-present");
        let excludes = pairs(&args, "--exclude");
        assert!(
            markers
                .contains(&"CACHEDIR.TAG:Signature: 8a477f597d28d172789f06886806bc55".to_owned())
        );
        for name in [
            "SourceAssetDB",
            "UnityLockfile",
            "CACHEDIR.TAG",
            "pyvenv.cfg",
        ] {
            assert!(excludes.contains(&name.to_owned()), "{name} missing");
        }
    }

    #[test]
    fn backup_filter_args_drops_built_ins_when_basic_excludes_are_off() {
        let config = AppConfig {
            use_basic_excludes: false,
            global_excludes: vec!["*.iso".to_owned()],
            ..AppConfig::default()
        };
        let args = backup_filter_args(&config, &[target(r"D:\p")]).unwrap();
        assert_eq!(args, vec!["--exclude".to_owned(), "*.iso".to_owned()]);
    }

    #[test]
    fn backup_filter_args_refuses_a_pattern_matching_the_target_or_a_parent() {
        let err = backup_filter_args(&AppConfig::default(), &[target(r"D:\target\proj")])
            .unwrap_err()
            .to_string();
        assert!(err.contains("「target/」"), "{err}");
        assert!(err.contains(r"D:\target\proj"), "{err}");
        assert!(err.contains("基本の除外パターン"), "{err}");

        // Environment-specific folders must not be excluded by default.
        for allowed in [r"D:\_Backups\proj", r"D:\.uloop\proj", r"D:\tmp-mock\proj"] {
            assert!(
                backup_filter_args(&AppConfig::default(), &[target(allowed)]).is_ok(),
                "expected {allowed} not to match BASIC_EXCLUDES"
            );
        }

        let mut own = target(r"D:\work\cache\notes");
        own.exclude_patterns = vec!["cache/".to_owned()];
        assert!(backup_filter_args(&AppConfig::default(), &[own]).is_err());
    }

    #[test]
    fn backup_filter_args_excludes_unity_folders_only_directly_under_a_project() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        let mkdir = |rel: &str| std::fs::create_dir_all(root.join(rel)).expect("mkdir");
        for project in ["World", r"repo\unity\Game"] {
            mkdir(&format!(r"{project}\ProjectSettings"));
            std::fs::write(
                root.join(project)
                    .join(r"ProjectSettings\ProjectVersion.txt"),
                "m_EditorVersion: 2022.3.22f1",
            )
            .expect("write ProjectVersion.txt");
        }
        // Generated folders directly under the projects, one in another case.
        for rel in [
            r"World\Build",
            r"World\obj",
            r"World\logs",
            r"World\Assets\SerializedUdonPrograms",
            r"repo\unity\Game\Library",
        ] {
            mkdir(rel);
        }
        // Real content that shares those names deeper down, and a project too deep to scan.
        for rel in [
            r"World\Packages\pkg\Editor\Build",
            r"World\Packages\pkg\Runtime\SerializedUdonPrograms",
            r"World\Assets\_Booth\x\Obj",
            r"World\Assets\Vendor\x\SerializedUdonPrograms",
            r"a\b\c\Deep\ProjectSettings",
            r"a\b\c\Deep\Build",
        ] {
            mkdir(rel);
        }
        std::fs::write(
            root.join(r"a\b\c\Deep\ProjectSettings\ProjectVersion.txt"),
            "m_EditorVersion: 2022.3.22f1",
        )
        .expect("write deep ProjectVersion.txt");

        let path = root.to_string_lossy().into_owned();
        let args = backup_filter_args(&AppConfig::default(), &[target(&path)]).unwrap();
        let mut anchored: Vec<String> = pairs(&args, "--exclude")
            .into_iter()
            .filter_map(|p| p.strip_prefix(&path).map(str::to_owned))
            .collect();
        anchored.sort();
        assert_eq!(
            anchored,
            [
                r"\World\Assets\SerializedUdonPrograms",
                r"\World\Build",
                r"\World\logs",
                r"\World\obj",
                r"\repo\unity\Game\Library",
            ]
        );
        let excludes = pairs(&args, "--exclude");
        for kept in [
            r"World\Packages\pkg\Editor\Build",
            r"World\Packages\pkg\Runtime\SerializedUdonPrograms",
            r"World\Assets\_Booth\x\Obj",
            r"World\Assets\Vendor\x\SerializedUdonPrograms",
        ] {
            let kept = root.join(kept).to_string_lossy().into_owned();
            assert!(
                !excludes.iter().any(|p| covers_path(p, &kept)),
                "{kept} would be excluded"
            );
        }

        let off = AppConfig {
            use_basic_excludes: false,
            ..AppConfig::default()
        };
        let args = backup_filter_args(&off, &[target(&path)]).unwrap();
        assert!(
            pairs(&args, "--exclude")
                .iter()
                .all(|p| !p.starts_with(&path))
        );
    }

    #[test]
    fn exclude_fingerprint_changes_only_with_the_exclude_settings() {
        let targets = [target(r"D:\p")];
        let base = exclude_fingerprint(&AppConfig::default(), &targets);
        assert_eq!(base, exclude_fingerprint(&AppConfig::default(), &targets));

        let added = AppConfig {
            global_excludes: vec!["*.iso".to_owned()],
            ..AppConfig::default()
        };
        let off = AppConfig {
            use_basic_excludes: false,
            ..AppConfig::default()
        };
        let mut own = target(r"D:\p");
        own.exclude_patterns = vec!["cache/".to_owned()];
        assert_ne!(base, exclude_fingerprint(&added, &targets));
        assert_ne!(base, exclude_fingerprint(&off, &targets));
        assert_ne!(base, exclude_fingerprint(&AppConfig::default(), &[own]));
    }

    #[test]
    fn backup_filter_args_accepts_real_target_paths_with_basic_excludes() {
        let targets = [
            r"D:\Projects\SampleWorld",
            r"D:\Projects\SampleAssetKit",
            r"E:\Media\Archives",
            r"D:\workspaces\web-app",
            r"C:\Users\me\Documents",
        ]
        .map(target);
        assert!(backup_filter_args(&AppConfig::default(), &targets).is_ok());
    }

    #[test]
    fn covers_path_follows_restic_pattern_rules() {
        assert!(covers_path("[Bb]uild/", r"D:\x\build\y"));
        assert!(!covers_path("[Bb]uild/", r"D:\x\BUILD\y"));
        assert!(covers_path("*.tmp", r"D:\a.tmp"));
        assert!(!covers_path("tmp_pack_*", r"D:\x\tmp-pack"));
        assert!(covers_path(".git/objects", r"D:\r\.git\objects\pack"));
        assert!(!covers_path(".git/objects", r"D:\r\.git"));
        assert!(covers_path(r"D:\Projects", r"D:\Projects\SampleProject"));
        assert!(!covers_path(
            r"D:\Projects\SampleProject\Temp",
            r"D:\Projects\SampleProject"
        ));
        assert!(covers_path("[^a]x", r"D:\bx"));
        assert!(!covers_path("[^a]x", r"D:\ax"));
        assert!(covers_path("[a-c]9", r"D:\b9"));
    }

    #[test]
    fn restore_delete_exclude_patterns_protects_unity_and_target_excludes() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(dir.path().join("ProjectSettings")).unwrap();
        std::fs::write(
            dir.path().join(r"ProjectSettings\ProjectVersion.txt"),
            "m_EditorVersion: 2022.3.22f1",
        )
        .unwrap();
        std::fs::create_dir_all(dir.path().join("Library")).unwrap();
        std::fs::create_dir_all(dir.path().join("CacheDir")).unwrap();
        std::fs::write(
            dir.path().join(r"CacheDir\CACHEDIR.TAG"),
            "Signature: 8a477f597d28d172789f06886806bc55\n",
        )
        .unwrap();
        std::fs::create_dir_all(dir.path().join("BogusCache")).unwrap();
        std::fs::write(
            dir.path().join(r"BogusCache\CACHEDIR.TAG"),
            "not a valid header",
        )
        .unwrap();

        let local_abs_exclude = dir.path().join("LocalCache").to_string_lossy().into_owned();
        let deep_cache = dir.path().join("a/b/c/d/e/f/g/DeepCache");
        std::fs::create_dir_all(&deep_cache).unwrap();
        std::fs::write(
            deep_cache.join("CACHEDIR.TAG"),
            "Signature: 8a477f597d28d172789f06886806bc55\n",
        )
        .unwrap();
        let bracket_cache = dir.path().join("[Cache]");
        std::fs::create_dir_all(&bracket_cache).unwrap();
        std::fs::write(
            bracket_cache.join("CACHEDIR.TAG"),
            "Signature: 8a477f597d28d172789f06886806bc55\n",
        )
        .unwrap();

        let rbracket_cache = dir.path().join("cache]");
        std::fs::create_dir_all(&rbracket_cache).unwrap();
        std::fs::write(
            rbracket_cache.join("CACHEDIR.TAG"),
            "Signature: 8a477f597d28d172789f06886806bc55\n",
        )
        .unwrap();

        let shared_abs = dir.path().join("SharedAbs").to_string_lossy().into_owned();
        let cfg = AppConfig {
            global_excludes: vec![shared_abs],
            ..AppConfig::default()
        };

        // Multibyte UTF-8 pattern longer than a short ASCII path must never panic on slice boundary.
        let patterns = restore_delete_exclude_patterns(
            &cfg,
            dir.path(),
            &[
                "*.log".to_owned(),
                "日本語除外パターン".to_owned(),
                local_abs_exclude,
            ],
            None,
        )
        .expect("restore excludes");

        for expected in [
            "/Library",
            "/Temp",
            "/Logs",
            "/obj",
            "/Build",
            "/Builds",
            "/Assets/SerializedUdonPrograms",
            "/CacheDir",
            "/a/b/c/d/e/f/g/DeepCache",
            "/[[]Cache]",
            "/cache]",
            "/SharedAbs",
            "*.log",
            "日本語除外パターン",
            "/LocalCache",
            "target/",
        ] {
            assert!(
                patterns.iter().any(|p| p == expected),
                "missing expected restore exclude {expected} in {patterns:?}"
            );
        }
        assert!(
            !patterns.iter().any(|p| p == "/BogusCache"),
            "invalid CACHEDIR.TAG header must not protect /BogusCache"
        );
        // UTF-8 boundary regression check directly against a short ASCII root path.
        assert_eq!(
            normalize_exclude_for_restore_root("日本語パターン", "D:/Proj"),
            vec!["日本語パターン".to_owned()]
        );
        // Glob prefix `Wor*/local-settings.json` matches
        // `World` case-sensitively and emits `/local-settings.json`, whereas wrong-case
        // `world/scene.txt` must not emit `/scene.txt` (Restic `--exclude` is case-sensitive).
        let rel_mapped =
            normalize_exclude_for_restore_root("World/local-settings.json", "D:/Projects/World");
        assert!(
            rel_mapped.iter().any(|p| p == "/local-settings.json"),
            "expected /local-settings.json in {rel_mapped:?}"
        );
        let rel_mapped_glob =
            normalize_exclude_for_restore_root("Wor*/local-settings.json", "D:/Projects/World");
        assert!(
            rel_mapped_glob.iter().any(|p| p == "/local-settings.json"),
            "expected /local-settings.json for glob prefix in {rel_mapped_glob:?}"
        );
        let rel_mapped_2 = normalize_exclude_for_restore_root(
            "Projects/World/local-settings.json",
            "D:/Projects/World",
        );
        assert!(
            rel_mapped_2.iter().any(|p| p == "/local-settings.json"),
            "expected /local-settings.json in {rel_mapped_2:?}"
        );
        let wrong_case = normalize_exclude_for_restore_root("world/scene.txt", "D:/Projects/World");
        assert!(
            !wrong_case.iter().any(|p| p == "/scene.txt"),
            "wrong-case exclude must NOT falsely exclude /scene.txt: {wrong_case:?}"
        );

        // Root directory carrying an exclude marker refuses restore.
        let marked_root = tempfile::tempdir().expect("marked root");
        std::fs::write(
            marked_root.path().join("CACHEDIR.TAG"),
            "Signature: 8a477f597d28d172789f06886806bc55\n",
        )
        .unwrap();
        assert!(
            restore_delete_exclude_patterns(&AppConfig::default(), marked_root.path(), &[], None)
                .is_err()
        );
        // Cancellation flag stops directory scan cleanly.
        let cancelled = AtomicBool::new(true);
        assert!(restore_delete_exclude_patterns(&cfg, dir.path(), &[], Some(&cancelled)).is_err());
    }
}
