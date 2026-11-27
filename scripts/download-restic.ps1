#requires -Version 7.0
<#
.SYNOPSIS
    Downloads the official restic Windows amd64 binary into binaries/restic.exe and verifies its SHA-256 checksum.
    For the pinned release (v0.20.0), verifies both the archive and extracted restic.exe hashes,
    including when reusing an already-cached binaries/restic.exe.
#>
[CmdletBinding()]
param(
    [string]$Version = "0.20.0",
    [switch]$VerifyOnly,
    [switch]$Force
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

# Pinned official hashes for the bundled restic Windows amd64 release.
$PinnedResticHashes = @{
    '0.20.0' = @{
        ZipSha256 = 'bba9061c3dce081701ab353ec8cb73a78be73b71a538aad560f2d1067602b7fe'
        ExeSha256 = '2683eb2b061f11568b9132ac0242f183ba4b3a0ed7630f1186a57e889c82a0e2'
    }
}

$repoRoot = Split-Path -Parent $PSScriptRoot
$binDir = Join-Path $repoRoot "binaries"
New-Item -ItemType Directory -Force -Path $binDir | Out-Null
$destExe = Join-Path $binDir "restic.exe"
$pinned = if ($PinnedResticHashes.ContainsKey($Version)) { $PinnedResticHashes[$Version] } else { $null }

if ((Test-Path -LiteralPath $destExe) -and -not $Force) {
    if ($null -ne $pinned) {
        $existingHash = (Get-FileHash -LiteralPath $destExe -Algorithm SHA256).Hash.ToLowerInvariant()
        if ($existingHash -eq $pinned.ExeSha256) {
            Write-Host "Verified cached restic.exe v$Version SHA-256: $existingHash" -ForegroundColor Green
            & $destExe version
            return
        }
        if ($VerifyOnly) {
            throw "Cached $destExe SHA-256 mismatch for pinned v$Version! Expected: $($pinned.ExeSha256), Actual: $existingHash"
        }
        Write-Host "Cached $destExe SHA-256 ($existingHash) does not match pinned v$Version ($($pinned.ExeSha256)); re-downloading..." -ForegroundColor Yellow
    } elseif ($VerifyOnly) {
        throw "No pinned restic.exe SHA-256 configured for version $Version"
    }
} elseif ($VerifyOnly) {
    throw "Expected sidecar binary not found at $destExe"
}

$zipName = "restic_${Version}_windows_amd64.zip"
$baseUrl = "https://github.com/restic/restic/releases/download/v${Version}"
$zipUrl = "${baseUrl}/${zipName}"
$sumsUrl = "${baseUrl}/SHA256SUMS"

$tmpDir = Join-Path ([System.IO.Path]::GetTempPath()) "restic-tray-dl-$PID"
New-Item -ItemType Directory -Force -Path $tmpDir | Out-Null

try {
    $zipPath = Join-Path $tmpDir $zipName
    $sumsPath = Join-Path $tmpDir "SHA256SUMS"

    Write-Host "Downloading $sumsUrl ..." -ForegroundColor Cyan
    Invoke-WebRequest -Uri $sumsUrl -OutFile $sumsPath -UseBasicParsing

    Write-Host "Downloading $zipUrl ..." -ForegroundColor Cyan
    Invoke-WebRequest -Uri $zipUrl -OutFile $zipPath -UseBasicParsing

    $sumsLines = Get-Content -Path $sumsPath
    $expectedLine = $sumsLines | Where-Object { $_ -match [regex]::Escape($zipName) } | Select-Object -First 1
    if (-not $expectedLine) {
        throw "Could not find checksum entry for $zipName in SHA256SUMS"
    }
    $expectedHash = ($expectedLine -split '\s+')[0].ToLowerInvariant()
    if ($null -ne $pinned -and $expectedHash -ne $pinned.ZipSha256) {
        throw "Upstream SHA256SUMS mismatch against pinned v$Version zip hash! Pinned: $($pinned.ZipSha256), Upstream: $expectedHash"
    }
    $actualHash = (Get-FileHash -LiteralPath $zipPath -Algorithm SHA256).Hash.ToLowerInvariant()

    if ($actualHash -ne $expectedHash) {
        throw "SHA-256 mismatch for $zipName! Expected: $expectedHash, Actual: $actualHash"
    }
    Write-Host "Archive SHA-256 verified: $actualHash" -ForegroundColor Green

    Expand-Archive -Path $zipPath -DestinationPath $tmpDir -Force
    $extractedExe = Join-Path $tmpDir "restic_${Version}_windows_amd64.exe"
    if (-not (Test-Path -LiteralPath $extractedExe)) {
        throw "Expected extracted binary not found at $extractedExe"
    }

    if ($null -ne $pinned) {
        $extractedHash = (Get-FileHash -LiteralPath $extractedExe -Algorithm SHA256).Hash.ToLowerInvariant()
        if ($extractedHash -ne $pinned.ExeSha256) {
            throw "Extracted binary SHA-256 mismatch for pinned v$Version! Expected: $($pinned.ExeSha256), Actual: $extractedHash"
        }
        Write-Host "Binary SHA-256 verified: $extractedHash" -ForegroundColor Green
    }

    Copy-Item -LiteralPath $extractedExe -Destination $destExe -Force
    Write-Host "Installed sidecar binary to $destExe" -ForegroundColor Green
    & $destExe version
}
finally {
    Remove-Item -Recurse -Force $tmpDir -ErrorAction SilentlyContinue
}
