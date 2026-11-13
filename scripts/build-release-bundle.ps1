# ==============================================================================
# Maya Downloader - Build Release Bundle (Windows x86_64)
# Usage:
#   .\scripts\build-release-bundle.ps1 [-Version <0.0.1>]
# ==============================================================================

[CmdletBinding()]
param (
    [string]$Version = "v0.0.1"
)

$ErrorActionPreference = "Stop"

$RootDir = Resolve-Path (Join-Path $PSScriptRoot "..")
Set-Location $RootDir

Write-Host "============================================================" -ForegroundColor Cyan
Write-Host "  Building Maya Release Bundle for Windows ($Version)       " -ForegroundColor Cyan
Write-Host "============================================================" -ForegroundColor Cyan

# 1. Compile release binaries
Write-Host "`n[1/5] Compiling release binaries with Cargo..." -ForegroundColor Yellow
$CargoOutput = cargo build --release -p maya-cli -p maya-daemon -p maya-nmh
if ($LASTEXITCODE -ne 0) {
    Write-Error "Cargo build failed with exit code $LASTEXITCODE"
}
Write-Host "[+] Cargo compilation completed successfully." -ForegroundColor Green

# 2. Setup Staging Directory
$BundleName = "maya-$Version-windows-x86_64"
$DistDir = Join-Path $RootDir "dist"
$StageDir = Join-Path $DistDir $BundleName

Write-Host "`n[2/5] Staging release contents into: $StageDir" -ForegroundColor Yellow
if (Test-Path $StageDir) {
    Remove-Item -Path $StageDir -Recurse -Force
}
New-Item -ItemType Directory -Path $StageDir -Force | Out-Null

# 3. Copy binaries and assets
Write-Host "`n[3/5] Copying binaries and distribution assets..." -ForegroundColor Yellow

$Binaries = @("maya.exe", "mayad.exe", "maya-nmh.exe")
foreach ($bin in $Binaries) {
    $src = Join-Path $RootDir "target\release\$bin"
    if (-not (Test-Path $src)) {
        Write-Error "Expected binary not found: $src"
    }
    Copy-Item -Path $src -Destination (Join-Path $StageDir $bin) -Force
    $binSize = (Get-Item $src).Length / 1MB
    Write-Host "  -> Copied $bin ($("{0:N2}" -f $binSize) MB)" -ForegroundColor Gray
}

# Copy documentation and installer
Copy-Item (Join-Path $RootDir "README.md") -Destination $StageDir -Force
Copy-Item (Join-Path $RootDir "LICENSE") -Destination $StageDir -Force
Copy-Item (Join-Path $RootDir "install.ps1") -Destination $StageDir -Force
Write-Host "  -> Copied README.md, LICENSE, install.ps1" -ForegroundColor Gray

# Copy browser extension
$ExtStageDir = Join-Path $StageDir "extensions\browser-capture"
New-Item -ItemType Directory -Path $ExtStageDir -Force | Out-Null
Copy-Item -Path (Join-Path $RootDir "extensions\browser-capture\*") -Destination $ExtStageDir -Recurse -Force
Write-Host "  -> Copied extensions/browser-capture" -ForegroundColor Gray

# 4. Create ZIP Archive
Write-Host "`n[4/5] Creating release zip archive..." -ForegroundColor Yellow
$ZipPath = Join-Path $DistDir "$BundleName.zip"
if (Test-Path $ZipPath) {
    Remove-Item -Path $ZipPath -Force
}

Compress-Archive -Path "$StageDir\*" -DestinationPath $ZipPath -CompressionLevel Optimal -Force
$ZipInfo = Get-Item $ZipPath
$ZipSizeMB = $ZipInfo.Length / 1MB
Write-Host "[+] Created release archive: $ZipPath ($("{0:N2}" -f $ZipSizeMB) MB)" -ForegroundColor Green

# 5. Calculate SHA256 Checksum
Write-Host "`n[5/5] Calculating SHA-256 checksum..." -ForegroundColor Yellow
$Hash = (Get-FileHash -Path $ZipPath -Algorithm SHA256).Hash.ToLower()
$ChecksumPath = "$ZipPath.sha256"
"$Hash  $BundleName.zip" | Out-File -FilePath $ChecksumPath -Encoding ascii -Force

Write-Host @"
============================================================
  RELEASE BUNDLE GENERATION COMPLETE!
============================================================
 Archive:   $ZipPath
 Size:      $("{0:N2}" -f $ZipSizeMB) MB ($($ZipInfo.Length) bytes)
 SHA-256:   $Hash
 Checksum:  $ChecksumPath
============================================================
"@ -ForegroundColor Green

return [PSCustomObject]@{
    Version   = $Version
    ArchivePath = $ZipPath
    SizeMB    = $ZipSizeMB
    Bytes     = $ZipInfo.Length
    SHA256    = $Hash
    ChecksumPath = $ChecksumPath
}
