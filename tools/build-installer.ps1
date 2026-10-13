# Build the release binaries and the installer: dist\simonaircast-setup-<version>.exe
# Requires Inno Setup 6 (`scoop install extras/inno-setup` provides iscc).
$ErrorActionPreference = 'Stop'
$repo = Split-Path $PSScriptRoot -Parent
Push-Location $repo
try {
    cargo build --release -p simonaircast
    if ($LASTEXITCODE -ne 0) { throw 'cargo build failed' }
    $version = (cargo metadata --format-version 1 --no-deps | ConvertFrom-Json).packages |
        Where-Object name -eq 'simonaircast' | Select-Object -ExpandProperty version
    iscc /Q "/DAppVersion=$version" installer\simonaircast.iss
    if ($LASTEXITCODE -ne 0) { throw 'iscc failed' }
    Get-Item "dist\simonaircast-setup-$version.exe"
} finally {
    Pop-Location
}
