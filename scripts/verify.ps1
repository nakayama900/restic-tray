#requires -Version 7.0
<#
.SYNOPSIS
    Local quality gate for Restic Tray (format, clippy, unit tests, real-Restic E2E, and cargo-deny).
    Pass -RequireAll (used by scripts/release.ps1) to fail if restic.exe or cargo-deny is missing.
#>
[CmdletBinding()]
param(
    [switch]$SkipDeny,
    [switch]$SkipResticE2e,
    [switch]$RequireAll
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

if ($RequireAll -and ($SkipDeny -or $SkipResticE2e)) {
    throw "-RequireAll cannot be combined with -SkipDeny or -SkipResticE2e"
}

$repoRoot = Split-Path -Parent $PSScriptRoot
Set-Location $repoRoot

$skipped = [System.Collections.Generic.List[string]]::new()

function Invoke-Step {
    param(
        [Parameter(Mandatory)][string]$Title,
        [Parameter(Mandatory)][scriptblock]$Action
    )
    Write-Host "=== $Title ===" -ForegroundColor Cyan
    & $Action
    if ($LASTEXITCODE -ne 0) {
        throw "Step failed ($LASTEXITCODE): $Title"
    }
}

function Find-ResticBinary {
    if ($env:RESTIC_TEST_BIN -and (Test-Path -LiteralPath $env:RESTIC_TEST_BIN)) {
        return $env:RESTIC_TEST_BIN
    }
    $sidecar = Join-Path $repoRoot 'binaries\restic.exe'
    if (Test-Path -LiteralPath $sidecar) {
        return $sidecar
    }
    $onPath = Get-Command restic -ErrorAction SilentlyContinue
    if ($onPath) {
        return $onPath.Source
    }
    return $null
}

Invoke-Step "[1/5] cargo fmt --all -- --check" {
    cargo fmt --all -- --check
}

Invoke-Step "[2/5] cargo clippy --locked --all-targets -- -D warnings" {
    cargo clippy --locked --all-targets -- -D warnings
}

Invoke-Step "[3/5] cargo test --locked" {
    cargo test --locked
}

$resticBin = Find-ResticBinary
if ($SkipResticE2e) {
    Write-Host "=== [4/5] real-Restic E2E skipped (-SkipResticE2e) ===" -ForegroundColor Yellow
    $skipped.Add("real-Restic E2E (-SkipResticE2e)")
} elseif ($resticBin) {
    Invoke-Step "[4/5] cargo test --locked real-Restic E2E ($resticBin)" {
        $e2eTest = 'controller::tests::end_to_end_two_pc_handoff_restore_delete_forget_and_legacy_with_real_restic'
        $e2eOut = & cargo test --locked --lib $e2eTest -- --ignored --exact 2>&1
        $e2eExit = $LASTEXITCODE
        $e2eOut | ForEach-Object { Write-Host $_ }
        if ($e2eExit -ne 0) {
            throw "real-Restic E2E test failed ($e2eExit)"
        }
        $joined = ($e2eOut | Out-String)
        if ($joined -notmatch 'test result: ok\. [1-9]\d* passed; 0 failed; 0 ignored;') {
            throw "real-Restic E2E test did not execute (expected >=1 passed test)"
        }
    }
} elseif ($RequireAll) {
    throw "Step failed: restic.exe is required for real-Restic E2E verification (-RequireAll). Run scripts/download-restic.ps1 first."
} else {
    Write-Host "=== [4/5] real-Restic E2E SKIPPED (restic.exe not found; run scripts/download-restic.ps1) ===" -ForegroundColor Yellow
    $skipped.Add("real-Restic E2E (restic.exe not found)")
}

$cargoDeny = Get-Command cargo-deny -ErrorAction SilentlyContinue
if ($SkipDeny) {
    Write-Host "=== [5/5] cargo-deny skipped (-SkipDeny) ===" -ForegroundColor Yellow
    $skipped.Add("cargo-deny (-SkipDeny)")
} elseif ($cargoDeny) {
    Invoke-Step "[5/5] cargo deny check advisories bans licenses sources" {
        cargo deny check advisories bans licenses sources
    }
} elseif ($RequireAll) {
    throw "Step failed: cargo-deny is required (-RequireAll). Install with 'cargo install cargo-deny --locked'."
} else {
    Write-Host "=== [5/5] cargo-deny SKIPPED (cargo-deny not installed) ===" -ForegroundColor Yellow
    $skipped.Add("cargo-deny (tool not installed)")
}

if ($skipped.Count -gt 0) {
    Write-Host "`nLocal checks passed with SKIPPED gates: $($skipped -join ', ')." -ForegroundColor Yellow
    Write-Host "Run with -RequireAll (and restic.exe + cargo-deny available) for full release verification." -ForegroundColor Yellow
} else {
    Write-Host "`nAll verification gates passed (fmt, clippy, unit tests, real-Restic E2E, cargo-deny)." -ForegroundColor Green
}
