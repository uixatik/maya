# ==============================================================================
# Maya Downloader - Windows PowerShell Installer
# Usage:
#   irm https://raw.githubusercontent.com/uixatik/maya/main/install.ps1 | iex
#   or run locally: .\install.ps1 [-Version <v0.0.1>] [-InstallDir <path>]
# ==============================================================================

[CmdletBinding()]
param (
    [string]$Version = "v0.0.1",
    [string]$InstallDir = "$env:LOCALAPPDATA\Programs\Maya\bin",
    [string]$Repo = "uixatik/maya",
    [switch]$Force,
    [switch]$SkipManifest,
    [switch]$InstallGui,
    [switch]$SkipGui
)

$ErrorActionPreference = "Stop"
if (-not $PSScriptRoot) {
    $PSScriptRoot = (Get-Location).Path
}

function Write-Step {
    param ([string]$Message)
    Write-Host "[*] $Message" -ForegroundColor Cyan
}

function Write-Success {
    param ([string]$Message)
    Write-Host "[+] $Message" -ForegroundColor Green
}

function Write-Warn {
    param ([string]$Message)
    Write-Host "[!] $Message" -ForegroundColor Yellow
}

function Write-Failure {
    param ([string]$Message)
    Write-Host "[-] $Message" -ForegroundColor Red
}

Write-Host @"
============================================================
   __  __                  ____                      _                 _ 
  |  \/  | __ _ _   _  __ |  _ \  _____      ___ __ | | ___   __ _  __| |
  | |\/| |/ _` | | | |/ _`| | | |/ _ \ \ /\ / / '_ \| |/ _ \ / _` |/ _` |
  | |  | | (_| | |_| | (_| | |_| | (_) \ V  V /| | | | | (_) | (_| | (_| |
  |_|  |_|\__,_|\__, |\__,_|____/ \___/ \_/\_/ |_| |_|_|\___/ \__,_|\__,_|
                |___/                                                    
============================================================
Fast, Resilient Multi-Protocol Downloader & Daemon
"@ -ForegroundColor Cyan

Write-Step "Target installation directory: $InstallDir"
if (-not (Test-Path -Path $InstallDir)) {
    New-Item -ItemType Directory -Path $InstallDir -Force | Out-Null
}

$Binaries = @("maya.exe", "mayad.exe", "maya-nmh.exe")
$SourceDir = $null

# 1. Check if binaries are available locally (current dir, dist, or target/release)
$Candidates = @(
    $PSScriptRoot,
    (Join-Path $PSScriptRoot "dist\maya-$Version-windows-x86_64"),
    (Join-Path $PSScriptRoot "target\release"),
    (Join-Path (Get-Location) "dist\maya-$Version-windows-x86_64"),
    (Join-Path (Get-Location) "target\release"),
    (Get-Location).Path
)

foreach ($Cand in $Candidates) {
    if ($Cand -and (Test-Path $Cand)) {
        $AllFound = $true
        foreach ($bin in $Binaries) {
            if (-not (Test-Path (Join-Path $Cand $bin))) {
                $AllFound = $false
                break
            }
        }
        if ($AllFound) {
            $SourceDir = $Cand
            Write-Success "Found local binaries in: $SourceDir"
            break
        }
    }
}

# 2. If not found locally, download release archive from GitHub Releases
if (-not $SourceDir) {
    Write-Step "Local binaries not found. Downloading release $Version from GitHub..."
    $ArchiveName = "maya-$Version-windows-x86_64.zip"
    $DownloadUrl = "https://github.com/$Repo/releases/download/$Version/$ArchiveName"
    $TempZip = Join-Path $env:TEMP $ArchiveName
    $TempExtract = Join-Path $env:TEMP "maya-extract-$Version"

    try {
        Write-Step "Downloading: $DownloadUrl"
        [Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12 -bor [Net.SecurityProtocolType]::Tls13
        Invoke-WebRequest -Uri $DownloadUrl -OutFile $TempZip -UseBasicParsing
        Write-Success "Downloaded release archive."

        if (Test-Path $TempExtract) {
            Remove-Item -Path $TempExtract -Recurse -Force
        }
        Expand-Archive -Path $TempZip -DestinationPath $TempExtract -Force
        
        # Check inside extracted dir
        if (Test-Path (Join-Path $TempExtract $Binaries[0])) {
            $SourceDir = $TempExtract
        } elseif (Test-Path (Join-Path $TempExtract "maya-$Version-windows-x86_64\$($Binaries[0])")) {
            $SourceDir = Join-Path $TempExtract "maya-$Version-windows-x86_64"
        } else {
            $Found = Get-ChildItem -Path $TempExtract -Recurse -Filter "maya.exe" | Select-Object -First 1
            if ($Found) {
                $SourceDir = $Found.DirectoryName
            }
        }
    } catch {
        Write-Warn "Could not download pre-built release binary ($($_.Exception.Message))."
        
        # Fallback: check if cargo is installed to build from source
        if (Get-Command cargo -ErrorAction SilentlyContinue) {
            Write-Step "Attempting to build from source using cargo..."
            cargo build --release -p maya-cli -p maya-daemon -p maya-nmh
            $TargetRelease = Join-Path (Get-Location) "target\release"
            if (Test-Path (Join-Path $TargetRelease "maya.exe")) {
                $SourceDir = $TargetRelease
            }
        }
    }
}

if (-not $SourceDir) {
    Write-Failure "Failed to locate or download Maya binaries. Please ensure binaries exist or Cargo is installed."
    exit 1
}

# 3. Copy binaries to destination
Write-Step "Installing binaries into $InstallDir..."
foreach ($bin in $Binaries) {
    $Src = Join-Path $SourceDir $bin
    if (Test-Path $Src) {
        Copy-Item -Path $Src -Destination (Join-Path $InstallDir $bin) -Force
        Write-Success "Installed $bin"
    } else {
        Write-Warn "Binary not found: $Src"
    }
}

# Copy browser extension bundle if present
$ExtSrcCandidates = @(
    (Join-Path $SourceDir "extensions\browser-capture"),
    (Join-Path $PSScriptRoot "extensions\browser-capture"),
    (Join-Path (Get-Location) "extensions\browser-capture")
)
$ExtDest = Join-Path (Split-Path $InstallDir -Parent) "extensions\browser-capture"
foreach ($extCand in $ExtSrcCandidates) {
    if (Test-Path $extCand) {
        New-Item -ItemType Directory -Path $ExtDest -Force | Out-Null
        Copy-Item -Path "$extCand\*" -Destination $ExtDest -Recurse -Force
        Write-Success "Installed browser extension to $ExtDest"
        break
    }
}

# 4. Add to User PATH if not already present
Write-Step "Configuring User PATH environment variable..."
$UserPath = [Environment]::GetEnvironmentVariable("Path", [EnvironmentVariableTarget]::User)
$PathEntries = $UserPath -split ";" | Where-Object { $_ -ne "" }

if ($PathEntries -notcontains $InstallDir) {
    $NewUserPath = "$UserPath;$InstallDir"
    [Environment]::SetEnvironmentVariable("Path", $NewUserPath, [EnvironmentVariableTarget]::User)
    Write-Success "Added $InstallDir to User PATH."
} else {
    Write-Success "$InstallDir is already in User PATH."
}

# Update current PowerShell session PATH
if ($env:PATH -notlike "*$InstallDir*") {
    $env:PATH = "$InstallDir;$env:PATH"
}

# 5. Automatically register Native Messaging Host manifests with browsers
if (-not $SkipManifest) {
    $NmhPath = Join-Path $InstallDir "maya-nmh.exe"
    if (Test-Path $NmhPath) {
        Write-Step "Registering Native Messaging Host manifests (Chrome, Edge, Firefox)..."
        try {
            & $NmhPath --install-manifest
            Write-Success "Browser extension manifests registered successfully!"
        } catch {
            Write-Warn "Could not automatically register browser manifests: $($_.Exception.Message)"
        }
    }
}

# 6. Verify Installation
Write-Host ""
Write-Step "Verifying installation..."
$MayaBin = Join-Path $InstallDir "maya.exe"
if (Test-Path $MayaBin) {
    try {
        $VerOutput = & $MayaBin --version
        Write-Success "Maya is ready: $VerOutput"
    } catch {
        Write-Warn "Could not execute $($MayaBin): $($_.Exception.Message)"
    }
}

# 7. Desktop GUI Installation Bridge
$ShouldPrompt = (-not $SkipGui) -and (-not $InstallGui) -and [Environment]::UserInteractive
$DoInstall = $InstallGui
if ($ShouldPrompt) {
    Write-Host ""
    $Answer = Read-Host "[?] Would you like to install the Maya Desktop GUI App as well? [Y/n]"
    if ([string]::IsNullOrWhiteSpace($Answer) -or $Answer.Trim().ToLower() -eq 'y' -or $Answer.Trim().ToLower() -eq 'yes') {
        $DoInstall = $true
    }
}

if ($DoInstall) {
    Write-Step "Preparing Maya Desktop GUI installer..."
    $SetupExeName = "Maya_0.0.1_x64-setup.exe"
    $LocalSetup = Join-Path $PSScriptRoot "dist\$SetupExeName"
    $LocalTargetSetup = Join-Path $PSScriptRoot "apps\maya-gui\src-tauri\target\release\bundle\nsis\$SetupExeName"
    $InstallerPath = $null
    if (Test-Path $LocalSetup) {
        $InstallerPath = $LocalSetup
    } elseif (Test-Path $LocalTargetSetup) {
        $InstallerPath = $LocalTargetSetup
    } else {
        $TempSetup = Join-Path $env:TEMP $SetupExeName
        $DownloadUrl = "https://github.com/$Repo/releases/download/$Version/$SetupExeName"
        Write-Step "Downloading Desktop GUI setup from $DownloadUrl..."
        Invoke-WebRequest -Uri $DownloadUrl -OutFile $TempSetup -UseBasicParsing
        $InstallerPath = $TempSetup
    }
    Write-Success "Launching Maya Desktop GUI setup wizard..."
    Start-Process -FilePath $InstallerPath
}

Write-Host @"

============================================================
 Maya Downloader Installation Complete!
============================================================
 Installed Binaries:
   - maya.exe     (CLI, TUI, batch, and queue client)
   - mayad.exe    (Background download daemon)
   - maya-nmh.exe (Browser Native Messaging Host)

 Directory: $InstallDir

 Quick Start:
   maya --help
   maya get "https://example.com/file.zip"
   maya tui
   maya daemon start

 Browser Extension:
   1. Open chrome://extensions or edge://extensions
   2. Turn on 'Developer mode'
   3. Click 'Load unpacked' and select:
      $ExtDest

============================================================
"@ -ForegroundColor Green
