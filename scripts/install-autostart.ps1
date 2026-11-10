<#
.SYNOPSIS
    Build, install and register PortalControl to start automatically at Windows login.

.DESCRIPTION
    1. Builds the release binary (skipped when -SkipBuild is given and a binary exists).
    2. Copies it to %LOCALAPPDATA%\PortalControl\bin\portalcontrol.exe, a stable
       location outside any synced folder.
    3. Registers it under HKCU\Software\Microsoft\Windows\CurrentVersion\Run
       (current user only, no administrator rights needed).
    4. Starts it in the background right away (unless -NoStart).

.EXAMPLE
    powershell -ExecutionPolicy Bypass -File scripts\install-autostart.ps1
.EXAMPLE
    powershell -ExecutionPolicy Bypass -File scripts\install-autostart.ps1 -Port 9000
.EXAMPLE
    powershell -ExecutionPolicy Bypass -File scripts\install-autostart.ps1 -Uninstall
#>
param(
    [int]$Port = 0,
    [string]$Bind = "",
    [switch]$SkipBuild,
    [switch]$NoStart,
    [switch]$Uninstall
)

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
$appDir = Join-Path $env:LOCALAPPDATA "PortalControl"
$binDir = Join-Path $appDir "bin"
$exe = Join-Path $binDir "portalcontrol.exe"
$buildDir = Join-Path $appDir "build"

$serveArgs = @()
if ($Bind) { $serveArgs += @("--bind", $Bind) }
if ($Port -gt 0) { $serveArgs += @("--port", "$Port") }

if ($Uninstall) {
    if (Test-Path $exe) {
        try { & $exe stop @serveArgs } catch { }
        & $exe uninstall-autostart
    } else {
        Remove-ItemProperty -Path "HKCU:\Software\Microsoft\Windows\CurrentVersion\Run" `
            -Name "PortalControl" -ErrorAction SilentlyContinue
        Write-Host "Autostart removed."
    }
    Write-Host "The binary and data in $appDir were left in place; delete that folder to remove everything."
    exit 0
}

if (-not ($SkipBuild -and (Test-Path $exe))) {
    if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
        throw "cargo was not found. Install Rust from https://rustup.rs and run this script again."
    }
    Write-Host "Building release binary..."
    $env:CARGO_TARGET_DIR = $buildDir
    Push-Location $root
    try {
        cargo build --release
        if ($LASTEXITCODE -ne 0) { throw "cargo build failed" }
    } finally {
        Pop-Location
    }
    # A running copy locks the exe; stop it before replacing the file.
    if (Test-Path $exe) {
        try { & $exe stop @serveArgs 2>$null | Out-Null } catch { }
        Start-Sleep -Milliseconds 800
    }
    New-Item -ItemType Directory -Force $binDir | Out-Null
    Copy-Item (Join-Path $buildDir "release\portalcontrol.exe") $exe -Force
    Write-Host "Installed $exe"
}

& $exe install-autostart @serveArgs
if ($LASTEXITCODE -ne 0) { throw "install-autostart failed" }

if (-not $NoStart) {
    & $exe serve --detach @serveArgs
    $shownPort = if ($Port -gt 0) { $Port } else { 7878 }
    $shownBind = if ($Bind) { $Bind } else { "127.0.0.1" }
    Write-Host "Dashboard: http://${shownBind}:$shownPort/"
}
