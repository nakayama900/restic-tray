# Contributing to Restic Tray

Thank you for your interest in contributing to **Restic Tray** (`nayuengin/restic-tray`)!

This repository is **operated and maintained by AI maintainers under the direction of the owner (`@nayuengin`)**. The AI maintainer charter, review standards, and authority boundaries are documented in [`AGENTS.md`](AGENTS.md). Human and AI-assisted contributions are both warmly welcome.

---

## Support Policy & Project Scope

Before investing time in a large change, please keep the project's scope in mind:

- **Supported version**: Latest release only (no backports to older versions).
- **No SLA**: Issue triage and PR review are best-effort with no guaranteed turnaround time.
- **Target scope**: Windows x64, Cloudflare R2 (`s3:` backend), and Japanese UI. Requests outside this scope (other OSes, alternative storage backends, localization) may be discussed in Issues, but implementation is not guaranteed.
- **When to open an Issue first**: Small bug fixes, test improvements, and documentation fixes can be submitted directly as a PR. For new features, MCP schema changes, or architectural changes, please open an Issue first to align on design before writing code.

---

## Contribution Workflow

1. **Check or open an Issue**: Search existing issues first. For non-trivial changes, open a Feature Request or Bug Report so the AI maintainer (or `@nayuengin`) can triage and align on scope (`Status: Ready`).
2. **Fork and branch**: Fork `nayuengin/restic-tray` and create a descriptive branch from `main` (e.g., `fix/exclude-trailing-slash`, `feat/mcp-filter-tags`).
3. **Implement and test locally**: Follow the [Development Environment Setup](#development-environment-setup) and preserve all [Core Engineering Invariants](#review-criteria).
4. **Run the full quality gate**: Run `pwsh -NoProfile -File scripts/verify.ps1 -RequireAll` (and `cargo audit --deny yanked` if you modified dependencies) before opening your PR.
5. **Open a Pull Request**: Fill out every section of [`.github/PULL_REQUEST_TEMPLATE.md`](.github/PULL_REQUEST_TEMPLATE.md), including the verification checklist and the **AI Assistance Disclosure** section.
6. **Automated checks & review**:
   - GitHub Actions runs CI (`Verify (Windows x64)`), Dependency Review, Secret Scan (`gitleaks`), RustSec Audit (`cargo-audit`), and automatic area labeling (`.github/labeler.yml`).
   - An AI maintainer or `@nayuengin` reviews your PR and marks `PR: AI Reviewed` when the AI review pass completes. PRs touching sensitive paths listed in [`.github/CODEOWNERS`](.github/CODEOWNERS) automatically receive `Status: Needs Human` (via `.github/labeler.yml`) and notify `@nayuengin` on external PRs for visibility (`CODEOWNERS` is used for ownership metadata, not as a merge-blocking review gate).
7. **Merge**: Once all [Merge Conditions](#merge-conditions) are met, a maintainer merges the PR into `main`.

---

## Reporting Issues

Use the structured forms under [Issues](https://github.com/nayuengin/restic-tray/issues/new/choose):

- **Bug Report**: Provide reproduction steps, expected vs. actual behavior, Restic Tray version (`restic-tray.exe --version`), Windows version, installation method, and redacted logs.
- **Feature Request**: Explain the concrete problem, proposed solution, and alternatives considered.
- **Question / Usage Help**: Ask usage or architecture questions after checking [`README.md`](README.md), [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md), and [`docs/AGENT.md`](docs/AGENT.md).
- **Security vulnerabilities**: **Never** open a public issue. Follow [`SECURITY.md`](SECURITY.md) and submit a private report via GitHub Private Vulnerability Reporting.

> [!CAUTION]
> **Never paste secrets or unredacted personal data into Issues or PRs.**
> `get_status` and `get_config` outputs include your Cloudflare R2 repository URL (Account ID, bucket, prefix) and local Windows file paths (which may include your Windows username). Always mask Account IDs (e.g., `<accountId>`), bucket/prefix names, and local usernames (`C:\Users\<user>`) before posting, and never paste R2 Access Keys, Secret Keys, repository passwords, or `agent-bridge.json` tokens.

---

## Labels & Triage Workflow

The canonical label set is defined in [`.github/labels.yml`](.github/labels.yml) and synchronized automatically by [`.github/workflows/labels.yml`](.github/workflows/labels.yml). PR area labels are applied automatically from changed file paths via [`.github/labeler.yml`](.github/labeler.yml).

### Label Taxonomy

| Category | Labels | Meaning |
| :--- | :--- | :--- |
| **Problem** | `Problem: Bug`, `Problem: Performance`, `Problem: Security` | Defects, performance regressions, and security hardening/advisories. |
| **Feedback** | `Feedback: Feature Request`, `Feedback: Question`, `Feedback: Discussion` | Feature ideas, usage questions, and design discussions. |
| **Scope** | `Scope: GUI`, `Scope: MCP`, `Scope: Restic`, `Scope: Scheduler`, `Scope: Packaging` | Affected subsystem (`eframe`/tray, `--mcp`/Named Pipe, Restic/R2/DPAPI/sync, scheduler/SQLite/controller, installers/scripts/assets). |
| **Priority** | `Priority: Critical`, `Priority: High`, `Priority: Medium`, `Priority: Low` | Urgency and user impact (`Critical` = security/data-loss/blocking crash; `Low` = minor polish). |
| **Status** | `Status: Triage`, `Status: Needs Info`, `Status: Needs Human`, `Status: Ready`, `Status: In Progress`, `Status: In Review`, `Status: Blocked`, `Status: Done` | Workflow state: newly opened (`Triage`), waiting on reporter (`Needs Info`), needs owner `@nayuengin`'s check or decision (`Needs Human`), actionable (`Ready`), active (`In Progress`), under review (`In Review`), waiting on dependency/decision (`Blocked`), or completed (`Done`). |
| **PR** | `PR: CI/Build`, `PR: Dependencies`, `PR: Documentation`, `PR: Refactor`, `PR: AI Authored`, `PR: AI Reviewed` | PR maintenance/content categories and AI governance flags (authored with AI, AI maintainer review completed). |
| **Close** | `Close: Duplicate`, `Close: Invalid`, `Close: Wontfix` | Resolution reasons for closed duplicates, invalid reports, or out-of-scope/declined items. |
| **Community** | `Community: Good First Issue`, `Community: Help Wanted` | Well-scoped tasks for first-time contributors and items welcoming community contributions. |

### Who Can Apply or Change Labels

- **Automation**: Issue Forms apply initial `Problem:` / `Feedback:` + `Status: Triage` labels; `actions/labeler` (`.github/workflows/labeler.yml`) applies `Scope:*` / `PR: Documentation` / `PR: CI/Build` / `Problem: Security` / `Status: Needs Human` labels on PRs from modified paths; Renovate (`.github/renovate.json`) applies `PR: Dependencies` (plus `Problem: Security` for vulnerability alerts, and `PR: Dependencies` + `PR: CI/Build` on the Dependency Dashboard issue).
- **Maintainers (Owner `@nayuengin` & AI Maintainers)**: Have permission to add, change, and remove all labels during triage, review, and escalation.
- **External Contributors**: Do not need to manage labels manually — selecting the appropriate Issue Form and filling out the PR template (including AI disclosure) gives automation and maintainers everything needed.

### How the AI Maintainer Uses Labels During Triage

1. **Incoming issues** arrive with `Status: Triage` and an initial `Problem:` or `Feedback:` label from the Issue Form.
2. **Safety check first**: If secrets or an unreported vulnerability appear in a public issue, the AI maintainer avoids repeating sensitive text, points to [`SECURITY.md`](SECURITY.md) / key rotation, and adds `Problem: Security` + `Status: Needs Human` for immediate owner attention.
3. **Classification**: The AI maintainer confirms the `Problem:` / `Feedback:` label, adds applicable `Scope:*` and `Priority:*` labels, removes `Status: Triage`, and moves the status to:
   - `Status: Needs Info` when reproduction steps, latest-version confirmation, or redacted logs are missing;
   - `Status: Ready` when the issue is reproducible, in scope, and ready to implement (adding `Community: Good First Issue` when well-scoped for newcomers);
   - `Status: Blocked` when waiting on an upstream fix, another issue, or an owner decision (paired with `Status: Needs Human` when `@nayuengin` must decide);
   - `Close: Duplicate`, `Close: Invalid`, or `Close: Wontfix` (for duplicates, invalid reports, or out-of-scope requests) with a clear explanation.
4. **Pull requests**: On PR review, the AI maintainer verifies the AI disclosure and ensures `PR: AI Authored` is set when AI tools authored or substantially generated the changes, adds `PR: AI Reviewed` once the AI review pass finishes, and ensures `Status: Needs Human` is set whenever sensitive paths listed in `.github/CODEOWNERS`, security boundaries, or scope decisions need `@nayuengin`'s attention or decision.

---

## Development Environment Setup

### Prerequisites (Windows x64)

1. **Rust stable toolchain** (`rust-toolchain.toml` pins `stable`, `rustfmt`, `clippy`, `x86_64-pc-windows-msvc`; MSRV **1.95**).
2. **Visual Studio Build Tools** with the MSVC x64 C++ workload.
3. **PowerShell 7 (`pwsh`)**.
4. **`cargo-deny`** (and optionally **`cargo-audit`**):
   ```powershell
   cargo install cargo-deny --locked
   cargo install cargo-audit --locked
   ```
5. **Pinned official `restic.exe`** (required for real-Restic end-to-end tests):
   ```powershell
   pwsh -NoProfile -File scripts/download-restic.ps1
   ```

### Useful Commands

```powershell
# Launch GUI + system tray + Named Pipe MCP bridge
cargo run

# Run MCP stdio server directly
cargo run -- --mcp

# Run the complete local quality gate (required for all PRs)
pwsh -NoProfile -File scripts/verify.ps1 -RequireAll

# Run RustSec advisory & yanked-crate audit (mirrors CI security.yml)
cargo audit --deny yanked

# Regenerate THIRD_PARTY_NOTICES.md after Cargo.lock or restic version updates
python scripts/generate-third-party-notices.py
```

---

## Pull Request Rules

### Review Criteria

Every pull request is evaluated against these criteria:

1. **All quality & security gates pass**:
   - Local `pwsh -NoProfile -File scripts/verify.ps1 -RequireAll` and CI `.github/workflows/ci.yml`:
     - `cargo fmt --all -- --check`
     - `cargo clippy --locked --all-targets -- -D warnings`
     - `cargo test --locked`
     - Real-Restic E2E test (`--locked`, `--ignored`)
     - `cargo deny check advisories bans licenses sources`
     - `python scripts/generate-third-party-notices.py --check` (run in CI)
   - Security workflow `.github/workflows/security.yml`:
     - Dependency Review (no newly introduced vulnerable dependencies)
     - Secret Scan (`gitleaks`)
     - RustSec Audit (`cargo audit --deny yanked`)
2. **Core engineering invariants are preserved**:
   - **No secrets in `config.json`, CLI args, or logs**: Secrets belong exclusively in Windows DPAPI `credentials.dat` (`src/secrets.rs`) and are passed to `restic.exe` only via environment variables (`src/restic/runner.rs`).
   - **No shell execution**: External commands (`restic.exe`) must use `std::process::Command` with argument arrays and `CREATE_NO_WINDOW` (never `cmd.exe` or `pwsh`).
   - **Path safety maintained**: Backup targets and restore destinations must pass through `src/fs_safety.rs` (`dunce::canonicalize` and system-path protection).
   - **MCP trust boundaries intact**: Destructive MCP tools (`forget_prune`, `delete_r2_bucket`, `takeover_project`) and prune-affecting MCP settings (`update_retention_policy`, `update_schedule` auto-prune / `maintenanceAt`) remain opt-in (`false` by default), destructive tools default to `dryRun: true`, `restore_snapshot` (enabled by default via `agent.allowRestore: true`) remains restricted to a separate validated `targetDir`, and secrets are never exposed or accepted over MCP.
   - **`unsafe_code = "deny"`**: `unsafe` is strictly restricted to Windows DPAPI FFI (`src/secrets.rs`) and main-window activation (`src/window.rs`), each with `#[allow(unsafe_code)]` and a `// SAFETY:` comment.
   - **Vector icons only**: Do not use emoji in UI screens; use `VectorIcon` in `src/theme/icons.rs`.
3. **Focused, self-contained diffs**: Keep each PR focused on one logical change. Add regression tests for bug fixes, keep `TOOLS` / `OPERATIONS` tests and `docs/AGENT.md` in sync for MCP changes, and update relevant docs (`docs/ARCHITECTURE.md`, `README*.md`).
4. **Dependency notices**: If `Cargo.lock` or bundled `restic.exe` changes, run `python scripts/generate-third-party-notices.py`, verify `python scripts/generate-third-party-notices.py --check` (checked in CI), and confirm `cargo deny check licenses` passes.
5. **No real personal data**: Tests, docs, and screenshots must use neutral synthetic examples (`pc-a`, `example-bucket`, fictional paths) and never real user paths, machine names, or credentials.

### Contributor Recognition & Changelog Credits

- **GitHub Release notes**: Draft releases automatically include GitHub's generated **What's Changed** (categorized by [`.github/release.yml`](.github/release.yml)) and **New Contributors** sections alongside the [`CHANGELOG.md`](CHANGELOG.md) excerpt.
- **Changelog acknowledgments**: External pull requests and bug reporters whose reports led to a fix are credited at the end of the corresponding [`CHANGELOG.md`](CHANGELOG.md) entry in the format `(#123, @user)` (where `#123` is the PR or issue number and `@user` is the contributor's GitHub username).

### Merge Conditions

All changes to `main` must go through a Pull Request (**direct pushes to `main` are prohibited**). A pull request may be merged into `main` only when **all** of the following hold:

1. **Pull Request + all required verification & security status checks are green**: `Verify (Windows x64)`, `Dependency Review`, `Secret Scan (Gitleaks)`, and `RustSec Audit (cargo-audit)` must all succeed. Merging with red, pending, skipped, or bypassed checks is strictly prohibited.
2. **PR template, AI disclosure, and commit rules are satisfied**: The PR template checklist and **AI Assistance Disclosure** are complete and consistent with the actual diff, open review feedback is resolved, and commit messages contain no AI attribution trailers (`Co-Authored-By`, etc.).

> [!NOTE]
> **No required review approvals or Code Owner review gating**: Because Restic Tray is operated by a solo owner (`@nayuengin`) together with AI maintainers under the owner's account, the `main` Ruleset requires Pull Requests and passing required status checks only — GitHub review approvals and Code Owner review checks are **not** required to merge. [`.github/CODEOWNERS`](.github/CODEOWNERS) is maintained for ownership visibility and external PR notifications (paired with automatic `Status: Needs Human` labeling in `.github/labeler.yml`).

---

## AI-Assisted Development Policy

**AI-assisted coding is fully permitted and welcomed in Restic Tray.**

Because this project is itself operated by AI maintainers under `@nayuengin`'s direction and ships a built-in MCP server for AI agents, we treat AI coding tools and autonomous agents as first-class development tools — on one clear condition: **transparency in the Pull Request**.

- **Mandatory PR disclosure (tool & scope)**: If you used AI coding tools or agents (e.g., Claude Code, Codex, Copilot, Cursor, Gemini, etc.), you **must** disclose **which AI tool(s)/model(s) were used** and **what scope they handled** (e.g., implementation, tests, refactoring, documentation) in the **AI Assistance Disclosure** section of the PR template. Maintainers will tag AI-authored PRs with `PR: AI Authored`.
- **Full contributor accountability**: You remain responsible for reading, understanding, and locally verifying every line submitted under your name. Unreviewed bulk dumps, hallucinated APIs, or unverified claims will be closed or returned for revision.
- **Grounded descriptions**: Keep PR descriptions concise and faithful to the actual diff and real command outputs.
- **No AI attribution in Git commits**: Do **not** include AI `Co-Authored-By` trailers or AI attribution lines in commit messages. AI usage disclosure belongs in the PR description and the `PR: AI Authored` label, not in Git commit metadata.

---

## Code of Conduct

All participants — human contributors and operators running AI agents — are expected to follow the [Code of Conduct](CODE_OF_CONDUCT.md).
