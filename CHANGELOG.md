# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

> Note: v1.0.0 is the first public release; entries prior to v1.0.0 document pre-publication development versions and are omitted from the public repository.

## [Unreleased]

### Added

- **macOS / Linux support (experimental, phase 1)**: the core app now builds and runs outside Windows.
  - Credentials: macOS Keychain and the Linux Secret Service (via the pure-Rust `keyring` crate); Windows keeps the DPAPI `credentials.dat` vault. On Linux, secret support requires a session D-Bus (`DBUS_SESSION_BUS_ADDRESS`).
  - App data: `~/Library/Application Support` (macOS) or `$XDG_DATA_HOME`/`~/.local/share` (Linux) instead of `%LOCALAPPDATA%`.
  - Single-instance and the resident MCP bridge use Unix-domain sockets instead of Windows named pipes (same mutual-token authentication and wire format).
  - Notifications: `osascript` user notifications on macOS, `notify-send` on Linux (argument arrays, no shell).
  - Restore safety: protected system trees (`/System`, `/usr`, `/etc`, `/boot`, `/var`, ...) are refused as restore destinations in addition to filesystem roots.
  - CI: experimental macOS and Ubuntu quality-gate jobs (fmt, clippy, unit tests) alongside the existing Windows gate. Packaging/installers remain Windows-only for now.

- **Rclone `serve s3` storage backend**: `storageBackend: "rclone"` runs `rclone serve s3 --addr 127.0.0.1:<port>` as a loopback-only anonymous local S3 adapter in front of any rclone remote (including `crypt:` remotes for client-side Rclone Crypto on top of Restic encryption), while Restic keeps using its standard S3 backend. Configurable `rclone.remote`, `rclone.bucket` (mapped to a directory under the remote root), fixed port, `rclone.exe` and `rclone.conf` overrides; the adapter restarts automatically when settings change and stops on app exit. R2 credentials are not required for this backend.
- **Gitignore & custom ignore files**: opt-in `useGitignore` honors each backup target's root `.gitignore`, and `ignoreFiles` adds custom gitignore-syntax ignore files (relative to each target or absolute). Patterns convert to Restic exclude rules (anchoring and `!` negations preserved) for backup, restore-overwrite protection and the excludes fingerprint.
- **Pkl configuration support**: when `config.json` does not exist, a sibling `config.pkl` (Apple Pkl) is evaluated with the pure-Rust `pklr` evaluator (network/package imports disabled). Saving continues to write `config.json`, which then takes precedence.
- MCP `update_storage_config` now accepts `storageBackend: "rclone"` and a `rclone` section; `get_status` reports the active backend.

## [1.0.0] - 2026-11-26

First public release of **Restic Tray** (`restic-tray`), a pure-Rust Windows system tray application for periodic client-side encrypted backups to Cloudflare R2 via [Restic](https://restic.net/), with a built-in Model Context Protocol (MCP) server.

### Added

- **Encrypted periodic backups to Cloudflare R2 via Restic**: Native `s3:` backend (`s3:https://<accountId>.r2.cloudflarestorage.com/<bucket>[/<prefix>]`) with client-side encryption, deduplication, per-target snapshots (`1 target = 1 snapshot`), and a bundled SHA-256-verified official `restic.exe` sidecar (v0.20.0).
- **Lightweight pure-Rust desktop GUI & system tray residency**: Built with `eframe`/`egui` 0.37 and `tray-icon` 0.26 (no WebView runtime) with Dark and Light themes, custom `VectorIcon`s, drag-and-drop folder enrollment, 7-day hourly coverage overview, and near-zero idle CPU usage while minimized to the tray.
- **Windows DPAPI credential isolation**: Encrypts Cloudflare R2 Access Key ID, Secret Access Key, and Restic repository password into `%LOCALAPPDATA%\io.github.nayuengin.restic-tray\config\credentials.dat` using Windows DPAPI (`CryptProtectData` with secondary entropy and `Zeroize`). Secrets are never written to `config.json` and are passed to `restic.exe` only through environment variables.
- **Smart excludes & declarative `config.json`**: Built-in regenerable cache/build excludes (`useBasicExcludes`), Unity project-root excludes (`Library`, `Temp`, `Logs`, `obj`, `Build`, `Builds`, `Assets\SerializedUdonPrograms`), marker-based `excludeIfPresent` (`SourceAssetDB`, `UnityLockfile`, `CACHEDIR.TAG`, `pyvenv.cfg`), per-target excludes/tags, and snapshot folder size breakdown.
- **Scheduling, integrity verification & notifications**: Daily time slots (`schedule.dailyAt`) or interval schedules, periodic read-back restore checks (`restic check --read-data-subset`, default 5% every 7 days), overdue Windows toast alerts (default 48 hours), retention policies (`keepLast` / `keepDaily` / `keepWeekly` / `keepMonthly`), and local SQLite (WAL) run history & audit log (`r2_restic_history.db`).
- **Snapshot browsing, diffing & safe restore**: Inspect snapshot files, diff snapshots, and restore full or selective paths (`includePaths`) with path canonicalization and drive-root / Windows system-directory protection (`src/fs_safety.rs`).
- **Built-in MCP server (`restic-tray.exe --mcp`)**: 22 stdio JSON-RPC 2.0 tools communicating with the resident tray app over a mutual-token-authenticated local Windows Named Pipe (`reject_remote_clients(true)`) with automatic headless fallback. Secrets are never returned or accepted over MCP; `restore_snapshot` is enabled by default for restoring into a separate validated `targetDir`, while destructive and prune-affecting operations (`forget_prune`, `update_retention_policy`, `autoPruneAfterBackup` / `maintenanceAt` via `update_schedule`, `delete_r2_bucket`, `takeover_project`) default to opt-in disabled and `dryRun: true`.
- **Safe Cloudflare R2 prefix & bucket cleanup**: Built-in pure-Rust S3 API client (`ureq` + Windows Schannel TLS + AWS SigV4 with `Zeroize`) with dry-run previews, exact bucket-name input, and a 1.5-second press-and-hold confirmation button in the GUI.
- **Multi-PC shared project handoff *(experimental)***: Single-writer coordination (`projectKey`, `writerState`, `release_project` / `takeover_project`) with subpath-rewritten restore, pre-takeover safety backups, and a dedicated primary maintenance window (`maintenanceRole`, `schedule.maintenanceAt`).
- **Verifiable Windows x64 bundles & automated upstream tracking**: NSIS installer (supports live overwrite while running), WiX MSI installer, and Portable ZIP published with `SHA256SUMS.txt` and GitHub Artifact Attestations, alongside a weekly workflow monitoring upstream `restic/restic` releases.

### Changed

- **Generalized built-in basic excludes (`BASIC_EXCLUDES`)**: Removed environment-specific folder names (`_Backups/`, `.uloop/`, `tmp-mock/`) from the default `useBasicExcludes` list so user directories with those names are not silently skipped. Add them to `globalExcludes` if you want them excluded.

### Fixed

- **Duration text layout in History**: Fixed CJK time units (`秒`, `分`, `時間`) colliding with preceding digits and dropping below the baseline when rendered in the History table.

### Supported Environment

- **OS**: Windows 10 / Windows Server 2016 or later (x64) with TLS 1.2 or later (required by bundled `restic.exe` v0.18.0+)
- **Storage**: Cloudflare R2 (S3-compatible object storage)

### Known Limitations

- **UI language**: The desktop GUI and built-in MCP tool descriptions are Japanese only.
- **Unsigned binaries**: Initial releases are unsigned; verify downloaded bundles with `SHA256SUMS.txt` and `gh attestation verify` before running (Windows Defender SmartScreen may show a warning).
- **Multi-PC shared project handoff**: Experimental pending extended multi-machine scheduled soak testing.
- **Sign-in auto-start**: Not yet built into Settings; place a shortcut to `restic-tray.exe --minimized` in `shell:startup` (see README Quick Start).

[Unreleased]: https://github.com/nayuengin/restic-tray/compare/v1.0.0...HEAD
[1.0.0]: https://github.com/nayuengin/restic-tray/releases/tag/v1.0.0
