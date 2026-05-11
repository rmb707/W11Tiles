# W11 Tiles — code-signing helper.
#
# Signs the daemon, CLI, and MSI in-place with a user-supplied certificate
# (either a .pfx file or a thumbprint already loaded into Cert:\CurrentUser\My).
#
# Usage examples:
#   .\installer\sign.ps1 -PfxPath C:\certs\rmb707.pfx -PfxPassword (Read-Host -AsSecureString)
#   .\installer\sign.ps1 -Thumbprint A1B2C3D4...
#
# Defaults to RFC 3161 timestamping via DigiCert's public server so signatures
# remain valid past cert expiry. signtool.exe must be on PATH (ships with the
# Windows SDK; install via `winget install Microsoft.WindowsSDK`).

[CmdletBinding(DefaultParameterSetName = 'Pfx')]
param(
    [Parameter(ParameterSetName = 'Pfx', Mandatory)]
    [string]$PfxPath,

    [Parameter(ParameterSetName = 'Pfx')]
    [System.Security.SecureString]$PfxPassword,

    [Parameter(ParameterSetName = 'Thumbprint', Mandatory)]
    [string]$Thumbprint,

    [string]$TimestampUrl = 'http://timestamp.digicert.com',
    [string]$Description  = 'W11 Tiles — native Windows 11 tiling window manager'
)

$ErrorActionPreference = 'Stop'

$signtool = Get-Command signtool.exe -ErrorAction SilentlyContinue
if (-not $signtool) {
    throw "signtool.exe not found on PATH. Install the Windows SDK: winget install Microsoft.WindowsSDK"
}

$repoRoot  = Split-Path -Parent $PSScriptRoot
$artifacts = @(
    Join-Path $repoRoot 'target\x86_64-pc-windows-gnu\release\tile-daemon.exe'
    Join-Path $repoRoot 'target\x86_64-pc-windows-gnu\release\tilectl.exe'
    Join-Path $repoRoot 'target\wix\W11Tiles-0.1.0-x64.msi'
) | Where-Object { Test-Path $_ }

if (-not $artifacts) {
    throw "No build artifacts found. Run `cargo build --release --target x86_64-pc-windows-gnu` and `wix build ...` first."
}

$args = @(
    'sign'
    '/fd', 'sha256'
    '/td', 'sha256'
    '/tr', $TimestampUrl
    '/d',  $Description
)

if ($PSCmdlet.ParameterSetName -eq 'Pfx') {
    $args += @('/f', $PfxPath)
    if ($PfxPassword) {
        $bstr = [Runtime.InteropServices.Marshal]::SecureStringToBSTR($PfxPassword)
        try {
            $plain = [Runtime.InteropServices.Marshal]::PtrToStringAuto($bstr)
            $args += @('/p', $plain)
        } finally {
            [Runtime.InteropServices.Marshal]::ZeroFreeBSTR($bstr)
        }
    }
} else {
    $args += @('/sha1', $Thumbprint)
}

foreach ($file in $artifacts) {
    Write-Host "Signing $file"
    & signtool.exe @args $file
    if ($LASTEXITCODE -ne 0) { throw "signtool failed for $file (exit $LASTEXITCODE)" }
}

Write-Host "`nAll artifacts signed." -ForegroundColor Green
foreach ($file in $artifacts) {
    & signtool.exe verify /pa /v $file | Select-String 'Successfully verified|Issued to|Issued by'
}
