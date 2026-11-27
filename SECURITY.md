# Security Policy

## Supported Versions

Only the **latest released version** of Restic Tray receives security fixes. Older versions are not patched or backported.

| Version | Supported |
| :--- | :--- |
| Latest release (`0.9.x`) | Supported |
| Older releases | Not supported |

---

## Reporting a Vulnerability

**Do not report security vulnerabilities through public GitHub Issues, Pull Requests, or Discussions.**

Report vulnerabilities privately using **GitHub Private Vulnerability Reporting**:

1. Direct link: [Report a vulnerability on `nayuengin/restic-tray`](https://github.com/nayuengin/restic-tray/security/advisories/new)
2. Or from the repository page on GitHub: open the **Security** tab → **Advisories** → click **Report a vulnerability**.

### What to Include

To help us reproduce, triage, and fix the issue quickly, please include:

- Affected Restic Tray version (`restic-tray.exe --version`) and Windows version (e.g., Windows 11 24H2 x64)
- Clear description of the vulnerability, affected component (`src/secrets.rs`, `src/fs_safety.rs`, `--mcp`, installer, etc.), and security impact
- Minimal reproduction steps or proof of concept
- Suggested mitigation or patch direction (optional)

> [!CAUTION]
> **Never include real credentials or personal data in a vulnerability report.**
> Use synthetic or redacted values for Cloudflare R2 Account IDs, bucket names, keys, repository passwords, and local paths.
>
> **If a secret or vulnerability was accidentally posted in a public Issue or PR**:
> 1. Immediately rotate/revoke any exposed Cloudflare R2 API tokens or Restic repository passwords.
> 2. Edit or delete the public comment to remove the sensitive text.
> 3. Open a private vulnerability report (or flag `@nayuengin`) so the maintainer can take any additional repository action needed.

---

## Response & Coordinated Disclosure Process

Restic Tray is maintained by a solo owner (`@nayuengin`) with AI-assisted maintenance on a best-effort basis and **has no formal SLA**. Private vulnerability reports are reviewed directly by the human owner (`@nayuengin`); user-supplied sensitive details are never published or shared with third parties.

We follow this concrete workflow (non-binding best-effort targets):

1. **Acknowledgment** *(target: within 7 days)*: Confirm receipt of your report in the private advisory thread.
2. **Triage & Scope Confirmation** *(target: within 14 days)*: Reproduce the issue, confirm whether it is in scope, and assess severity.
3. **Private Remediation**: Develop and verify the fix privately inside a GitHub Security Advisory temporary private fork, running the full quality gate (`scripts/verify.ps1 -RequireAll`) and sandbox smoke tests (`scripts/release.ps1 smoke`) before release.
4. **Coordinated Release & Disclosure**: Publish a patched release (with `SHA256SUMS.txt` and GitHub Artifact Attestation), then publish the GitHub Security Advisory (and request a CVE via GitHub where applicable). Reporter credit is included if requested.

---

## Scope

### In Scope

Vulnerabilities that break Restic Tray's documented security invariants (see [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md)):

- **Credential leakage**: Exposure of Cloudflare R2 keys or Restic repository passwords in `config.json`, command-line arguments, logs, SQLite history, crash output, or MCP responses.
- **MCP local trust-boundary bypass**: Remote network access to the Named Pipe bridge, authentication token bypass, or unintended escalation of destructive operations (`forget_prune`, `delete_r2_bucket`, `takeover_project`) or prune-affecting settings (`update_retention_policy`, `update_schedule` auto-prune / `maintenanceAt`) when opt-in flags are disabled or `dryRun` is expected.
- **Command or argument injection**: Shell invocation or argument injection when invoking `restic.exe` (e.g., snapshot ID or path handling).
- **Path traversal & unsafe restore**: Bypassing `src/fs_safety.rs` canonicalization, reparse-point checks, drive-root blocks, or Windows system-directory protection.
- **Supply-chain / packaging integrity**: Tampered or mismatched `restic.exe` SHA-256 pinning, installer privilege/path-hijacking issues, or workflow permission escalation.

### Out of Scope

- **Compromised same-user session**: An attacker who already has arbitrary code execution under the **same Windows user account** can access user-scoped DPAPI secrets and `%LOCALAPPDATA%\io.github.nayuengin.restic-tray\agent-bridge.json` by Windows OS design.
- **External account compromise**: Compromised Cloudflare accounts, R2 buckets, or API keys leaked outside Restic Tray.
- **Windows SmartScreen / Smart App Control warnings**: Initial releases are intentionally distributed **unsigned**; see [`README.md`](README.md#verifying-downloads-checksums--build-provenance) for checksum and Artifact Attestation verification instructions.
- **Upstream `restic` vulnerabilities**: Please report vulnerabilities in `restic` itself directly to the [restic security policy](https://github.com/restic/restic/security/policy). Once an upstream fix is released, we will update the pinned `restic.exe` version and SHA-256 hash in `scripts/download-restic.ps1`.

---

## Security Model & Release Verification

Key architectural defenses implemented in Restic Tray:

- **Credential isolation**: Cloudflare R2 keys and the Restic repository password are encrypted using Windows DPAPI (`CryptProtectData` with secondary entropy and `Zeroize`) into `config\credentials.dat`, scoped to the current Windows user account. They are never stored in `config.json`, never passed as CLI arguments, and never exposed or accepted over MCP.
- **MCP local trust boundary**: The Windows Named Pipe bridge rejects remote clients (`reject_remote_clients(true)`) and requires a per-run 256-bit mutual authentication token stored in `%LOCALAPPDATA%\io.github.nayuengin.restic-tray\agent-bridge.json`, verified in constant time.
- **No shell execution**: `restic.exe` is invoked directly via `std::process::Command` with argument arrays (`CREATE_NO_WINDOW`), validated snapshot IDs, and `--` positional separators. `resticBinaryPath` cannot be modified via MCP.
- **Path & restore safety**: Backup targets and restore destinations are canonicalized via `src/fs_safety.rs`, rejecting `..` traversal, unresolved reparse points, drive roots, and Windows system directories.
- **Destructive operation guards**: Destructive MCP tools (`forget_prune`, `delete_r2_bucket`, `takeover_project`) are opt-in (`false` by default), default to `dryRun: true`, and require explicit confirmation fields. While `agent.allowPrune` is `false`, MCP also blocks `update_retention_policy`, enabling `autoPruneAfterBackup`, setting or changing `maintenanceAt`, and post-backup auto-pruning from MCP-initiated backups. `restore_snapshot` is enabled by default (`agent.allowRestore: true`) and restores into a separate validated `targetDir` protected by `src/fs_safety.rs` rather than overwriting the original location.
- **Verifiable CI releases**: Official release binaries are built on GitHub-hosted runners (`.github/workflows/release.yml`) with `SHA256SUMS.txt` and GitHub Artifact Attestations (`gh attestation verify <file> --repo nayuengin/restic-tray`).
