#requires -Version 7.0
<#
.SYNOPSIS
    Local release pipeline orchestrator for Restic Tray.
    Builds release binary + NSIS setup + WiX MSI + Portable ZIP (all bundling restic.exe),
    runs installer smoke tests, generates SHA256SUMS.txt, and publishes to GitHub Releases.
#>
[CmdletBinding()]
param(
    [Parameter(Position = 0)]
    [ValidateSet('build', 'smoke', 'checksums', 'publish', 'release')]
    [string]$Subcommand = 'build',

    [string]$Tag,
    [string]$NotesFile = 'CHANGELOG.md',
    [switch]$Draft,
    [switch]$SkipVerify,
    [switch]$SkipIcon,
    [switch]$SkipBuild,
    [switch]$SkipSmoke,
    [switch]$NoUpload
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$repoRoot = Split-Path -Parent $PSScriptRoot
Set-Location $repoRoot

$cargoTomlPath = Join-Path $repoRoot 'Cargo.toml'
$bundleRoot = Join-Path $repoRoot 'target\release\bundle'
$nsisDir = Join-Path $bundleRoot 'nsis'
$msiDir = Join-Path $bundleRoot 'msi'
$portableDir = Join-Path $bundleRoot 'portable'
$checksumsPath = Join-Path $bundleRoot 'SHA256SUMS.txt'

function Get-CurrentVersion {
    $text = Get-Content -LiteralPath $cargoTomlPath -Raw -Encoding UTF8
    $m = [regex]::Match($text, '(?ms)^\[package\].*?^version\s*=\s*"(?<ver>\d+\.\d+\.\d+)"')
    if (-not $m.Success) {
        throw "Could not parse [package].version from $cargoTomlPath"
    }
    return $m.Groups['ver'].Value
}

function Resolve-ToolPath {
    param([string]$CommandName, [string[]]$FallbackPaths)
    $cmd = Get-Command $CommandName -ErrorAction SilentlyContinue
    if ($cmd) { return $cmd.Source }
    foreach ($p in $FallbackPaths) {
        if (Test-Path -LiteralPath $p) { return $p }
    }
    throw "Required tool '$CommandName' was not found in PATH or fallback locations: $($FallbackPaths -join ', ')"
}

function Get-ExpectedBundlePaths {
    param([Parameter(Mandatory)][string]$Version)
    return @(
        (Join-Path $nsisDir "Restic.Tray_${Version}_x64-setup.exe"),
        (Join-Path $msiDir "Restic.Tray_${Version}_x64_ja-JP.msi"),
        (Join-Path $portableDir "Restic.Tray_${Version}_x64-portable.zip")
    )
}

function Get-BundleArtifacts {
    param([string]$Version = (Get-CurrentVersion))
    $expectedPaths = Get-ExpectedBundlePaths -Version $Version
    $missing = @($expectedPaths | Where-Object { -not (Test-Path -LiteralPath $_ -PathType Leaf) })
    if ($missing.Count -gt 0) {
        throw "Missing expected v$Version release artifact(s): $($missing -join ', '). Run 'build' first."
    }
    return @($expectedPaths | ForEach-Object { Get-Item -LiteralPath $_ }) | Sort-Object Name
}

function Assert-PublishArtifactsValid {
    param([Parameter(Mandatory)][string]$Tag)

    if ($Tag -notmatch '^v(?<ver>\d+\.\d+\.\d+)$') {
        throw "publish requires -Tag in 'vX.Y.Z' format (got '$Tag')"
    }
    $tagVer = $Matches['ver']
    $manifestVer = Get-CurrentVersion
    if ($tagVer -ne $manifestVer) {
        throw "Tag '$Tag' does not match Cargo.toml version 'v$manifestVer'"
    }

    if (-not (Test-Path -LiteralPath $checksumsPath -PathType Leaf)) {
        throw "SHA256SUMS.txt missing at $checksumsPath. Run 'checksums' first."
    }

    $bundles = @(Get-BundleArtifacts -Version $manifestVer)
    $expectedByName = @{}
    foreach ($file in $bundles) {
        $expectedByName[$file.Name.Replace(' ', '.')] = $file
    }

    $recordedHashes = @{}
    $lines = @(Get-Content -LiteralPath $checksumsPath -Encoding UTF8 | Where-Object { $_.Trim().Length -gt 0 })
    if ($lines.Count -eq 0) {
        throw "SHA256SUMS.txt is empty at $checksumsPath"
    }
    foreach ($line in $lines) {
        if ($line -notmatch '^(?<hash>[0-9a-fA-F]{64})  (?<name>\S+)$') {
            throw "Malformed line in SHA256SUMS.txt: '$line'"
        }
        $name = $Matches['name']
        $hash = $Matches['hash'].ToLowerInvariant()
        if ($recordedHashes.ContainsKey($name)) {
            throw "Duplicate entry for '$name' in SHA256SUMS.txt"
        }
        $recordedHashes[$name] = $hash
    }

    $expectedNames = @($expectedByName.Keys | Sort-Object)
    $recordedNames = @($recordedHashes.Keys | Sort-Object)
    if (($expectedNames -join ',') -ne ($recordedNames -join ',')) {
        throw "SHA256SUMS.txt entries [$($recordedNames -join ', ')] do not match expected v$manifestVer artifacts [$($expectedNames -join ', ')]"
    }

    foreach ($assetName in $expectedNames) {
        $file = $expectedByName[$assetName]
        $actualHash = (Get-FileHash -Algorithm SHA256 -LiteralPath $file.FullName).Hash.ToLowerInvariant()
        if ($actualHash -ne $recordedHashes[$assetName]) {
            throw "SHA-256 mismatch for $assetName! SHA256SUMS.txt=$($recordedHashes[$assetName]), actual=$actualHash. Re-run 'checksums'."
        }
    }

    Write-Host "Verified $Tag matches v$manifestVer with all $($bundles.Count) artifacts and SHA256SUMS.txt" -ForegroundColor Green
    return $bundles
}

function Invoke-Build {
    $ver = Get-CurrentVersion
    $resticSidecar = Join-Path $repoRoot 'binaries\restic.exe'
    Write-Host "=== Verifying / downloading pinned official restic.exe sidecar ===" -ForegroundColor Cyan
    & pwsh -NoProfile -File "$PSScriptRoot\download-restic.ps1"
    if ($LASTEXITCODE -ne 0) { throw "download-restic.ps1 failed" }

    if (-not $SkipIcon) {
        Write-Host "=== Generating deterministic app icons ===" -ForegroundColor Cyan
        & python "$PSScriptRoot\generate-icon.py"
        if ($LASTEXITCODE -ne 0) { throw "generate-icon.py failed" }
    }

    if (-not $SkipVerify) {
        & pwsh -NoProfile -File "$PSScriptRoot\verify.ps1" -RequireAll
        if ($LASTEXITCODE -ne 0) { throw "verify.ps1 failed" }
    }

    Write-Host "=== Building release binary (v$ver) ===" -ForegroundColor Cyan
    & cargo build --release --locked
    if ($LASTEXITCODE -ne 0) { throw "cargo build --release --locked failed" }

    $releaseExe = Join-Path $repoRoot 'target\release\restic-tray.exe'
    if (-not (Test-Path -LiteralPath $releaseExe)) {
        throw "Release binary not found at $releaseExe"
    }

    foreach ($dir in @($nsisDir, $msiDir, $portableDir)) {
        if (Test-Path -LiteralPath $dir) {
            Remove-Item -LiteralPath $dir -Recurse -Force
        }
        New-Item -ItemType Directory -Force -Path $dir | Out-Null
    }

    # 1. Build NSIS Installer
    $makensis = Resolve-ToolPath -CommandName 'makensis.exe' -FallbackPaths @(
        "$env:LOCALAPPDATA\tauri\NSIS\makensis.exe",
        "C:\Program Files (x86)\NSIS\makensis.exe",
        "C:\Program Files\NSIS\makensis.exe"
    )
    $nsisOut = Join-Path $nsisDir "Restic.Tray_${ver}_x64-setup.exe"
    Write-Host "=== Building NSIS installer -> $nsisOut ===" -ForegroundColor Cyan
    & $makensis `
        "/DVERSION=$ver" `
        "/DOUT_FILE=$nsisOut" `
        "/DBIN_PATH=$releaseExe" `
        "/DRESTIC_PATH=$resticSidecar" `
        "/DICON_PATH=$repoRoot\assets\icon.ico" `
        "/DLICENSE_PATH=$repoRoot\LICENSE" `
        "/DNOTICES_PATH=$repoRoot\THIRD_PARTY_NOTICES.md" `
        "/DREADME_PATH=$repoRoot\README.md" `
        "$repoRoot\packaging\nsis\installer.nsi"
    if ($LASTEXITCODE -ne 0) { throw "makensis failed" }

    # 2. Build WiX MSI Installer
    $candle = Resolve-ToolPath -CommandName 'candle.exe' -FallbackPaths @(
        "$env:LOCALAPPDATA\tauri\WixTools314\candle.exe"
    )
    $light = Resolve-ToolPath -CommandName 'light.exe' -FallbackPaths @(
        "$env:LOCALAPPDATA\tauri\WixTools314\light.exe"
    )
    $wixObj = Join-Path $msiDir 'main.wixobj'
    $msiOut = Join-Path $msiDir "Restic.Tray_${ver}_x64_ja-JP.msi"
    Write-Host "=== Building WiX MSI installer -> $msiOut ===" -ForegroundColor Cyan
    & $candle `
        "-dVersion=$ver" `
        "-dBinPath=$releaseExe" `
        "-dResticPath=$resticSidecar" `
        "-dIconPath=$repoRoot\assets\icon.ico" `
        "-dLicensePath=$repoRoot\LICENSE" `
        "-dNoticesPath=$repoRoot\THIRD_PARTY_NOTICES.md" `
        "-dReadmePath=$repoRoot\README.md" `
        -arch x64 `
        -out $wixObj `
        "$repoRoot\packaging\wix\main.wxs"
    if ($LASTEXITCODE -ne 0) { throw "WiX candle.exe failed" }

    & $light -ext WixUIExtension -cultures:ja-jp -spdb -sice:ICE38 -sice:ICE64 -sice:ICE91 -out $msiOut $wixObj
    if ($LASTEXITCODE -ne 0) { throw "WiX light.exe failed" }
    Remove-Item -LiteralPath $wixObj -Force -ErrorAction SilentlyContinue

    # 3. Build Portable ZIP
    $portableZip = Join-Path $portableDir "Restic.Tray_${ver}_x64-portable.zip"
    Write-Host "=== Building Portable ZIP -> $portableZip ===" -ForegroundColor Cyan
    $stageDir = Join-Path $portableDir 'stage'
    New-Item -ItemType Directory -Force -Path $stageDir | Out-Null
    Copy-Item -LiteralPath $releaseExe -Destination $stageDir
    Copy-Item -LiteralPath $resticSidecar -Destination $stageDir
    Copy-Item -LiteralPath "$repoRoot\LICENSE" -Destination $stageDir
    Copy-Item -LiteralPath "$repoRoot\THIRD_PARTY_NOTICES.md" -Destination $stageDir
    Copy-Item -LiteralPath "$repoRoot\README.md" -Destination $stageDir
    Compress-Archive -Path "$stageDir\*" -DestinationPath $portableZip -Force
    Remove-Item -LiteralPath $stageDir -Recurse -Force

    Write-Host "Built 3 release bundles for v$ver under $bundleRoot" -ForegroundColor Green
}

function Invoke-Smoke {
    # In Windows Sandbox, so the install on this PC is never replaced or removed.
    & pwsh -NoProfile -File "$PSScriptRoot\sandbox-smoke.ps1" -Version (Get-CurrentVersion)
    if ($LASTEXITCODE -ne 0) { throw "Installer smoke test failed" }
}

function Invoke-Checksums {
    $bundles = @(Get-BundleArtifacts)
    if ($bundles.Count -eq 0) {
        throw "No bundles found under $bundleRoot. Run 'build' first."
    }
    $lines = @()
    foreach ($file in $bundles) {
        $hash = (Get-FileHash -Algorithm SHA256 -LiteralPath $file.FullName).Hash.ToLowerInvariant()
        $assetName = $file.Name.Replace(' ', '.')
        $line = "$hash  $assetName"
        $lines += $line
        Write-Host $line
    }
    Set-Content -LiteralPath $checksumsPath -Value (($lines -join "`n") + "`n") -Encoding UTF8 -NoNewline
    Write-Host "Wrote $checksumsPath ($($bundles.Count) entries)" -ForegroundColor Green
}

function Extract-ChangelogSection {
    param([string]$ChangelogPath, [string]$Version)
    if (-not (Test-Path -LiteralPath $ChangelogPath)) { return $null }
    $raw = Get-Content -LiteralPath $ChangelogPath -Raw -Encoding UTF8
    $pattern = "(?ms)^## \[$([regex]::Escape($Version))\][^\r\n]*\r?\n(?<body>.*?)(?=^## |^\[[^\]]+\]:\s|\z)"
    $m = [regex]::Match($raw, $pattern)
    if (-not $m.Success) { return $null }
    $tmpNotes = Join-Path $bundleRoot "release-notes-v$Version.md"
    Set-Content -LiteralPath $tmpNotes -Value ($m.Groups['body'].Value.Trim() + "`n") -Encoding UTF8 -NoNewline
    return $tmpNotes
}

function Invoke-Publish {
    if (-not $Tag) {
        throw "publish requires -Tag <vX.Y.Z>"
    }
    $bundles = @(Assert-PublishArtifactsValid -Tag $Tag)
    $ver = $Tag.TrimStart('v')

    & gh release view $Tag *> $null
    if ($LASTEXITCODE -ne 0) {
        & git rev-parse -q --verify "refs/tags/$Tag" *> $null
        if ($LASTEXITCODE -ne 0) {
            Write-Host "Creating annotated git tag $Tag..." -ForegroundColor Cyan
            & git tag -a $Tag -m "Release $Tag"
            if ($LASTEXITCODE -ne 0) { throw "git tag failed for $Tag" }
        }
        Write-Host "Pushing tag $Tag to origin..." -ForegroundColor Cyan
        & git push origin $Tag
        if ($LASTEXITCODE -ne 0) { throw "git push origin $Tag failed" }

        $createArgs = @('release', 'create', $Tag, '--title', $Tag, '--verify-tag')
        $notesSection = Extract-ChangelogSection -ChangelogPath (Join-Path $repoRoot $NotesFile) -Version $ver
        if ($notesSection) {
            $createArgs += @('--notes-file', $notesSection)
        } else {
            $createArgs += '--generate-notes'
        }
        if ($Draft) { $createArgs += '--draft' }
        Write-Host "Creating GitHub Release $Tag..." -ForegroundColor Cyan
        & gh @createArgs
        if ($LASTEXITCODE -ne 0) { throw "gh release create failed" }
    } else {
        Write-Host "Release $Tag already exists; uploading assets with --clobber..." -ForegroundColor Cyan
    }

    $uploadPaths = @($bundles | ForEach-Object { $_.FullName }) + @($checksumsPath)
    & gh release upload $Tag @uploadPaths --clobber
    if ($LASTEXITCODE -ne 0) { throw "gh release upload failed" }
    Write-Host "Uploaded $($uploadPaths.Count) assets to GitHub Release $Tag" -ForegroundColor Green
}

switch ($Subcommand) {
    'build' { Invoke-Build }
    'smoke' { Invoke-Smoke }
    'checksums' { Invoke-Checksums }
    'publish' { Invoke-Publish }
    'release' {
        $ver = Get-CurrentVersion
        $relTag = if ($Tag) { $Tag } else { "v$ver" }
        if (-not $SkipBuild) { Invoke-Build }
        if (-not $SkipSmoke) { Invoke-Smoke }
        Invoke-Checksums
        if (-not $NoUpload) {
            $Tag = $relTag
            Invoke-Publish
        }
    }
}
