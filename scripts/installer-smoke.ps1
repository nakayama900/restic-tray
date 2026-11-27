#requires -Version 5.1
<#
.SYNOPSIS
    Installer smoke test for a single Windows bundle (NSIS or MSI): silent
    install, verify embedded binary version, bundled restic.exe sidecar, & MCP
    stdio response, start the app and check that it idles in the tray (NSIS),
    then silent uninstall and clean-state verification.
    Runs under Windows PowerShell 5.1 too, which is all Windows Sandbox has;
    scripts/sandbox-smoke.ps1 runs it there.
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory)]
    [ValidateSet('nsis', 'msi')]
    [string]$Type,

    [Parameter(Mandatory)]
    [ValidateScript({ Test-Path -LiteralPath $_ -PathType Leaf })]
    [string]$InstallerPath,

    [Parameter(Mandatory)]
    [ValidatePattern('^\d+\.\d+\.\d+$')]
    [string]$ExpectedVersion,

    [string]$ProductName = 'Restic Tray',
    [string]$MainBinaryName = 'restic-tray',
    [string]$LogPath = "$PSScriptRoot\..\target\release\bundle\installer-smoke.log",
    [switch]$KeepInstalled
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$installDir = Join-Path $env:LOCALAPPDATA "Programs\$ProductName"
$installedExe = Join-Path $installDir "$MainBinaryName.exe"
$installedRestic = Join-Path $installDir "restic.exe"
$uninstRegPath = "HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall\$ProductName"

function Write-SmokeLog {
    param([string]$Message)
    $line = "[$(Get-Date -Format 'yyyy-MM-dd HH:mm:ss')] [$Type] $Message"
    Write-Host $line -ForegroundColor Gray
    $parent = Split-Path -Parent $LogPath
    if ($parent -and -not (Test-Path $parent)) {
        New-Item -ItemType Directory -Force -Path $parent | Out-Null
    }
    Add-Content -LiteralPath $LogPath -Value $line -Encoding UTF8
}

function Assert-VersionAndMcp {
    param([string]$ExePath, [string]$Expected)
    $cliVer = (& $ExePath --version | Out-String).Trim()
    if ($cliVer -notmatch [regex]::Escape($Expected)) {
        # Exit code -1073741515 (0xC0000135) means Windows could not find a DLL the app needs.
        throw "CLI --version mismatch: expected '$Expected', got '$cliVer' (exit code $LASTEXITCODE)"
    }
    Write-SmokeLog "CLI --version OK: $cliVer"

    if (-not (Test-Path -LiteralPath $installedRestic)) {
        throw "Bundled restic.exe missing in install directory: $installedRestic"
    }
    Write-SmokeLog "Bundled restic.exe present: $installedRestic"

    $initReq = '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18"}}'
    $mcpOut = ($initReq | & $ExePath --mcp | Out-String).Trim()
    if ($mcpOut -notmatch '"restic-tray"') {
        throw "MCP --mcp initialize failed: $mcpOut"
    }
    Write-SmokeLog "MCP stdio initialize OK"
}

# Closing the window moves the app to the tray, where it must use no CPU. The window's event
# loop once kept running there and used a whole core, which no unit test can catch.
function Assert-IdleInTray {
    param([string]$ExePath)
    Add-Type -Namespace SmokeTest -Name Win32 -MemberDefinition @'
[DllImport("user32.dll", CharSet = CharSet.Unicode)] public static extern IntPtr FindWindowExW(IntPtr parent, IntPtr after, string cls, string title);
[DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr hwnd, out uint pid);
[DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr hwnd);
[DllImport("user32.dll")] public static extern bool PostMessageW(IntPtr hwnd, uint msg, IntPtr w, IntPtr l);
'@
    $wmClose = 0x10
    $idleSeconds = 10
    $allowedCpuSeconds = 1.0

    # Its own data folder, which also gives it its own single-instance pipe.
    $dataDir = Join-Path $env:TEMP "restic-tray-smoke-$PID"
    New-Item -ItemType Directory -Force -Path $dataDir | Out-Null
    $realLocalAppData = $env:LOCALAPPDATA
    $env:LOCALAPPDATA = $dataDir
    try { $app = Start-Process -FilePath $ExePath -PassThru } finally { $env:LOCALAPPDATA = $realLocalAppData }
    try {
        $hwnd = [IntPtr]::Zero
        $deadline = (Get-Date).AddSeconds(60)
        while ($true) {
            if ($app.HasExited) { throw "The app exited while starting (exit code $($app.ExitCode))" }
            # Another copy of the app may have a window with the same title, so check the owner.
            $found = [IntPtr]::Zero
            do {
                $found = [SmokeTest.Win32]::FindWindowExW([IntPtr]::Zero, $found, [NullString]::Value, $ProductName)
                [uint32]$owner = 0
                [void][SmokeTest.Win32]::GetWindowThreadProcessId($found, [ref]$owner)
                if ($owner -eq $app.Id -and [SmokeTest.Win32]::IsWindowVisible($found)) { $hwnd = $found }
            } while ($found -ne [IntPtr]::Zero -and $hwnd -eq [IntPtr]::Zero)
            if ($hwnd -ne [IntPtr]::Zero) { break }
            if ((Get-Date) -gt $deadline) { throw "The app window did not appear within 60 seconds" }
            Start-Sleep -Milliseconds 500
        }
        Write-SmokeLog "App window shown"

        Start-Sleep -Seconds 2
        [void][SmokeTest.Win32]::PostMessageW($hwnd, $wmClose, [IntPtr]::Zero, [IntPtr]::Zero)
        Start-Sleep -Seconds 5
        if ($app.HasExited) { throw "Closing the window ended the app instead of moving it to the tray" }
        if ([SmokeTest.Win32]::IsWindowVisible($hwnd)) { throw "The window is still shown after closing it" }

        $app.Refresh()
        $before = $app.TotalProcessorTime
        Start-Sleep -Seconds $idleSeconds
        $app.Refresh()
        $used = ($app.TotalProcessorTime - $before).TotalSeconds
        if ($used -gt $allowedCpuSeconds) {
            throw ("The app used {0:N2} s of CPU time in {1} s while idle in the tray" -f $used, $idleSeconds)
        }
        Write-SmokeLog ("Idle in the tray: {0:N2} s of CPU time in {1} s" -f $used, $idleSeconds)
    }
    finally {
        if (-not $app.HasExited) {
            $app.Kill()
            $app.WaitForExit()
        }
        Remove-Item -LiteralPath $dataDir -Recurse -Force -ErrorAction SilentlyContinue
    }
}

# The test installs into the per-user install folder and then uninstalls from it, so on a PC
# where the app is in use it would replace and then remove that install.
if ((Test-Path -LiteralPath $installedExe) -or (Test-Path -LiteralPath $uninstRegPath)) {
    throw "$ProductName is already installed at $installDir. This smoke test would overwrite and then uninstall it. Run it through scripts/sandbox-smoke.ps1, or on a PC where the app is not installed."
}

$installerFull = (Resolve-Path -LiteralPath $InstallerPath).Path
Write-SmokeLog "Starting installer smoke test for $installerFull"

if ($Type -eq 'nsis') {
    $p = Start-Process -FilePath $installerFull -ArgumentList @('/S') -Wait -PassThru
    if ($p.ExitCode -ne 0) {
        throw "NSIS silent install failed with exit code $($p.ExitCode)"
    }
    if (-not (Test-Path -LiteralPath $installedExe)) {
        throw "Installed binary missing after NSIS install: $installedExe"
    }
    if (-not (Test-Path -LiteralPath $uninstRegPath)) {
        throw "Uninstall registry key missing after NSIS install: $uninstRegPath"
    }

    Assert-VersionAndMcp -ExePath $installedExe -Expected $ExpectedVersion
    Assert-IdleInTray -ExePath $installedExe

    # Install again while the app runs, as an update does. Windows lets a running exe be renamed
    # but not overwritten or deleted, so the installer has to move it aside.
    $startInfo = New-Object System.Diagnostics.ProcessStartInfo
    $startInfo.FileName = $installedExe
    $startInfo.Arguments = '--mcp'
    $startInfo.UseShellExecute = $false
    $startInfo.RedirectStandardInput = $true
    $startInfo.RedirectStandardOutput = $true
    $running = [System.Diagnostics.Process]::Start($startInfo)
    try {
        $again = Start-Process -FilePath $installerFull -ArgumentList @('/S') -Wait -PassThru
        if ($again.ExitCode -ne 0) {
            throw "NSIS install over a running copy failed with exit code $($again.ExitCode)"
        }
        $aside = @(Get-ChildItem -LiteralPath $installDir -Filter "$MainBinaryName.exe.*.old")
        if ($aside.Count -ne 1) {
            throw "Expected the running exe to be moved aside as one .old file, found $($aside.Count)"
        }
    }
    finally {
        $running.Kill()
        $running.WaitForExit()
    }
    Assert-VersionAndMcp -ExePath $installedExe -Expected $ExpectedVersion
    Write-SmokeLog "Install over a running copy moved the old exe aside: $($aside[0].Name)"

    if (-not $KeepInstalled) {
        $uninstaller = Join-Path $installDir 'uninstall.exe'
        $up = Start-Process -FilePath $uninstaller -ArgumentList @('/S', "_?=$installDir") -Wait -PassThru
        if ($up.ExitCode -ne 0) {
            throw "NSIS silent uninstall failed with exit code $($up.ExitCode)"
        }
        Remove-Item -LiteralPath $uninstaller -Force -ErrorAction SilentlyContinue
        Remove-Item -LiteralPath $installDir -Force -ErrorAction SilentlyContinue
    }
}
elseif ($Type -eq 'msi') {
    $msiInstallLog = "$LogPath.msi-install.log"
    $msiUninstallLog = "$LogPath.msi-uninstall.log"
    $p = Start-Process -FilePath 'msiexec.exe' -ArgumentList @('/i', "`"$installerFull`"", '/qn', '/norestart', '/l*v', "`"$msiInstallLog`"") -Wait -PassThru
    if ($p.ExitCode -ne 0) {
        throw "MSI silent install failed with exit code $($p.ExitCode)"
    }
    if (-not (Test-Path -LiteralPath $installedExe)) {
        throw "Installed binary missing after MSI install: $installedExe"
    }

    Assert-VersionAndMcp -ExePath $installedExe -Expected $ExpectedVersion

    if (-not $KeepInstalled) {
        $up = Start-Process -FilePath 'msiexec.exe' -ArgumentList @('/x', "`"$installerFull`"", '/qn', '/norestart', '/l*v', "`"$msiUninstallLog`"") -Wait -PassThru
        if ($up.ExitCode -ne 0) {
            throw "MSI silent uninstall failed with exit code $($up.ExitCode)"
        }
    }
}

if (-not $KeepInstalled) {
    Start-Sleep -Seconds 1
    if (Test-Path -LiteralPath $installedExe) {
        throw "Installed binary still present after uninstall: $installedExe"
    }
    $leftOver = @(Get-ChildItem -LiteralPath $installDir -Filter '*.old' -ErrorAction SilentlyContinue)
    if ($leftOver.Count -gt 0) {
        throw "Moved-aside binaries still present after uninstall: $($leftOver.Name -join ', ')"
    }
}

Write-SmokeLog "Smoke test passed cleanly for $Type ($ExpectedVersion)."
