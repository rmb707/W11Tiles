# W11 Tiles — installer build helper.
#
# Builds release binaries and produces the per-user MSI at:
#   target\wix\W11Tiles-<version>-x64.msi
#
# Usage:
#   .\installer\build-msi.ps1                # uses workspace version
#   .\installer\build-msi.ps1 -Version 0.1.1 # override

[CmdletBinding()]
param(
    [string]$Version,
    [string]$Target = 'x86_64-pc-windows-gnu'
)

$ErrorActionPreference = 'Stop'
$repoRoot = Split-Path -Parent $PSScriptRoot
Set-Location $repoRoot

if (-not $Version) {
    $cargoToml = Get-Content (Join-Path $repoRoot 'Cargo.toml') -Raw
    if ($cargoToml -match '(?m)^version\s*=\s*"([^"]+)"') {
        $Version = $Matches[1]
    } else {
        throw "Could not detect version from Cargo.toml; pass -Version explicitly."
    }
}

Write-Host "Building W11 Tiles v$Version for $Target..." -ForegroundColor Cyan
cargo build --release --target $Target
if ($LASTEXITCODE -ne 0) { throw "cargo build failed" }

$daemonExe = "target\$Target\release\tile-daemon.exe"
$tilectlExe = "target\$Target\release\tilectl.exe"
foreach ($exe in @($daemonExe, $tilectlExe)) {
    if (-not (Test-Path $exe)) { throw "Missing build artifact: $exe" }
}

New-Item -ItemType Directory -Force -Path "target\wix" | Out-Null

$msiOut = "target\wix\W11Tiles-$Version-x64.msi"
Write-Host "`nBuilding MSI -> $msiOut" -ForegroundColor Cyan
wix build "installer\wix\W11Tiles.wxs" `
    -arch x64 `
    -ext WixToolset.UI.wixext `
    -d "Version=$Version" `
    -d "DaemonExe=$daemonExe" `
    -d "TilectlExe=$tilectlExe" `
    -d "LicensePath=installer\wix\License.rtf" `
    -o $msiOut
if ($LASTEXITCODE -ne 0) { throw "wix build failed" }

$size = [math]::Round((Get-Item $msiOut).Length / 1MB, 2)
Write-Host "`nDone. $msiOut ($size MB)" -ForegroundColor Green
Write-Host "Sign it with: .\installer\sign.ps1 -PfxPath <path-to-cert.pfx>" -ForegroundColor DarkGray
