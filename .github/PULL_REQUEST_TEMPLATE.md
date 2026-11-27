## Summary

<!-- Briefly explain what this PR changes and why. -->

Closes #

## Type of Change

- [ ] Bug fix (`Problem: Bug`)
- [ ] Performance improvement (`Problem: Performance`)
- [ ] New feature / enhancement (`Feedback: Feature Request`)
- [ ] Refactoring (`PR: Refactor`)
- [ ] Documentation (`PR: Documentation`)
- [ ] Maintenance / CI / build (`PR: CI/Build`)
- [ ] Dependencies (`PR: Dependencies`)
- [ ] Security hardening (`Problem: Security`)

## Affected Area

- [ ] Desktop GUI / Tray / Notifications (`Scope: GUI`)
- [ ] MCP Server / Named Pipe Bridge (`Scope: MCP`)
- [ ] Restic / Cloudflare R2 / Secrets / Sync (`Scope: Restic`)
- [ ] Scheduler / SQLite History / Config (`Scope: Scheduler`)
- [ ] Installer / Packaging / Scripts (`Scope: Packaging`)

## Verification Gate

Run `pwsh -NoProfile -File scripts/verify.ps1 -RequireAll` locally before opening this PR:

- [ ] `cargo fmt --all -- --check` passed
- [ ] `cargo clippy --all-targets -- -D warnings` passed
- [ ] `cargo test` passed
- [ ] Real-Restic E2E test passed (`binaries/restic.exe` or `restic` on `PATH`)
- [ ] `cargo deny check advisories bans licenses sources` passed
- [ ] `cargo audit --deny yanked` passed (if `Cargo.toml` / `Cargo.lock` changed)
- [ ] `THIRD_PARTY_NOTICES.md` regenerated (if `Cargo.lock` or bundled `restic.exe` changed)
- [ ] Documentation updated (`docs/AGENT.md`, `docs/ARCHITECTURE.md`, `README*.md`, `CHANGELOG.md` as applicable)

## Security & Core Invariants

- [ ] No secrets written to `config.json`, CLI arguments, or logs (DPAPI `credentials.dat` + env vars only)
- [ ] External commands invoked without a shell (`Command` argument array + `CREATE_NO_WINDOW`)
- [ ] Path normalization and restore safety preserved (`src/fs_safety.rs`)
- [ ] MCP trust boundaries preserved (destructive ops and prune-affecting config changes remain opt-in `false` by default, destructive tools default to `dryRun: true`, and `restore_snapshot` stays isolated to a separate validated `targetDir`)
- [ ] No new `unsafe` outside approved DPAPI / Win32 window-activation sites
- [ ] No emoji icons in UI screens (`VectorIcon` in `src/theme/icons.rs` only)
- [ ] No real user paths, machine names, R2 Account IDs, or personal data in code, tests, docs, or screenshots

## AI Assistance Disclosure

> AI-assisted coding is fully permitted and welcomed in this repository (see [`CONTRIBUTING.md`](https://github.com/nayuengin/restic-tray/blob/main/CONTRIBUTING.md#ai-assisted-development-policy)), provided you disclose the AI tool(s) and scope below.

- [ ] This PR **did not** use AI coding tools or agents
- [ ] This PR **used** AI coding tools or agents (fill in the tool/model and scope below; maintainers will apply `PR: AI Authored`)

- **AI tool(s) / model(s) used**: <!-- e.g., Claude Code (Claude Opus), Codex, GitHub Copilot, Gemini, or N/A -->
- **Scope handled by AI**: <!-- e.g., Rust implementation, unit tests, refactoring, documentation, review, or N/A -->

- [ ] I have read, understood, and locally verified every change in this PR.
- [ ] I confirmed that commit messages contain **no** AI co-author trailers (`Co-Authored-By`, etc.).

## Screenshots (if UI changed)

<!-- Attach before/after screenshots using synthetic demo data only (no credentials, no real paths). -->
