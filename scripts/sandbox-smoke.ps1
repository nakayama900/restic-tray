#requires -Version 7.0
<#
.SYNOPSIS
    Runs installer-smoke.ps1 for the MSI and NSIS bundles inside Windows Sandbox, a throwaway
    Windows where the app is not installed, so the test never touches the install on this PC.
    Needs the "Windows Sandbox" optional feature with its `wsb` command line.
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory)]
    [ValidatePattern('^\d+\.\d+\.\d+$')]
    [string]$Version,

    [int]$TimeoutMinutes = 15,
    [switch]$KeepOpen
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$repoRoot = Split-Path -Parent $PSScriptRoot
$bundleRoot = Join-Path $repoRoot 'target\release\bundle'
$outDir = Join-Path $bundleRoot 'sandbox-smoke'
$msi = "msi\Restic.Tray_${Version}_x64_ja-JP.msi"
$nsis = "nsis\Restic.Tray_${Version}_x64-setup.exe"

foreach ($rel in @($msi, $nsis)) {
    if (-not (Test-Path -LiteralPath (Join-Path $bundleRoot $rel) -PathType Leaf)) {
        throw "Installer not found: $(Join-Path $bundleRoot $rel). Run 'pwsh scripts/release.ps1 build' first."
    }
}
$wsb = Get-Command wsb.exe -ErrorAction SilentlyContinue
if (-not $wsb) {
    throw "The Windows Sandbox command line (wsb.exe) was not found. Turn on the 'Windows Sandbox' optional feature and update it from the Microsoft Store, or pass -SkipSmoke to release.ps1."
}
$wsb = $wsb.Source

if (Test-Path -LiteralPath $outDir) { Remove-Item -LiteralPath $outDir -Recurse -Force }
New-Item -ItemType Directory -Force -Path $outDir | Out-Null

# Runs inside the sandbox under Windows PowerShell 5.1 and leaves its verdict in `result.txt`.
$entry = @"
`$ErrorActionPreference = 'Stop'
`$result = 'PASS'
try {
    & C:\scripts\installer-smoke.ps1 -Type msi -InstallerPath 'C:\bundle\$msi' -ExpectedVersion '$Version' -LogPath 'C:\out\installer-smoke.log'
    & C:\scripts\installer-smoke.ps1 -Type nsis -InstallerPath 'C:\bundle\$nsis' -ExpectedVersion '$Version' -LogPath 'C:\out\installer-smoke.log'
} catch {
    `$result = "FAIL: `$(`$_.Exception.Message)"
}
Set-Content -LiteralPath 'C:\out\result.txt' -Value `$result -Encoding UTF8
"@
Set-Content -LiteralPath (Join-Path $outDir 'entry.ps1') -Value $entry -Encoding UTF8

function Get-MappedFolder {
    param([string]$HostFolder, [string]$SandboxFolder, [string]$ReadOnly)
    "<MappedFolder><HostFolder>$HostFolder</HostFolder><SandboxFolder>$SandboxFolder</SandboxFolder><ReadOnly>$ReadOnly</ReadOnly></MappedFolder>"
}
# Only C:\out is writable, and the sandbox has no network.
$config = '<Configuration><Networking>Disable</Networking><MappedFolders>' +
    (Get-MappedFolder $bundleRoot 'C:\bundle' 'true') +
    (Get-MappedFolder $PSScriptRoot 'C:\scripts' 'true') +
    (Get-MappedFolder $outDir 'C:\out' 'false') +
    '</MappedFolders></Configuration>'

Write-Host "=== Starting Windows Sandbox (installer smoke test for v$Version) ===" -ForegroundColor Cyan
$started = (& $wsb start --raw --config $config) -join ''
$id = try { ($started | ConvertFrom-Json).Id } catch { $null }
if (-not $id) { throw "Windows Sandbox did not start: $started" }

$result = "FAIL: the sandbox user session did not become ready within $TimeoutMinutes minutes"
try {
    # The per-user installers need a signed-in user, which exists only once a window is connected.
    Start-Process -FilePath $wsb -ArgumentList @('connect', '--id', $id) | Out-Null
    $command = 'powershell.exe -NoProfile -ExecutionPolicy Bypass -File C:\out\entry.ps1'
    $deadline = (Get-Date).AddMinutes($TimeoutMinutes)
    while ((Get-Date) -lt $deadline) {
        $ran = (& $wsb exec --id $id --raw -r ExistingLogin -c $command 2>&1) -join ' '
        if ($ran -match '"ExitCode"') {
            $resultPath = Join-Path $outDir 'result.txt'
            $result = if (Test-Path -LiteralPath $resultPath) {
                (Get-Content -LiteralPath $resultPath -Raw -Encoding UTF8).Trim()
            } else {
                "FAIL: the test script wrote no result ($ran)"
            }
            break
        }
        Start-Sleep -Seconds 5
    }
}
finally {
    # Stop it through wsb: killing the sandbox processes leaves its virtual machine running.
    if (-not $KeepOpen) { & $wsb stop --id $id | Out-Null }
}

$logPath = Join-Path $outDir 'installer-smoke.log'
if (Test-Path -LiteralPath $logPath) { Get-Content -LiteralPath $logPath -Encoding UTF8 | Write-Host }
if ($result -ne 'PASS') {
    throw "Sandbox smoke test failed. $result (log: $logPath)"
}
Write-Host "Sandbox smoke test passed for MSI and NSIS (v$Version)." -ForegroundColor Green
