# Build a release in dist/:
#
#   pwsh scripts/package-windows.ps1
#
#   dist/Spotlight-<version>-Setup.exe     the installer, which most people want
#   dist/Spotlight-<version>-Portable.exe  runs from anywhere and keeps its settings beside itself
#
# The installer needs Inno Setup 6: winget install JRSoftware.InnoSetup
$ErrorActionPreference = "Stop"
$root = Split-Path $PSScriptRoot -Parent
Push-Location $root
try {
    cargo build --release
    if ($LASTEXITCODE -ne 0) { throw "build failed" }
    $version = (Select-String -Path Cargo.toml -Pattern '^version = "(.+)"' | Select-Object -First 1).Matches[0].Groups[1].Value
    $dist = Join-Path $root "dist"
    Remove-Item $dist -Recurse -Force -ErrorAction SilentlyContinue
    New-Item -ItemType Directory -Force $dist | Out-Null

    # Portable: the same exe. "Portable" in its name is what makes it keep settings beside itself.
    $portable = Join-Path $dist "Spotlight-$version-Portable.exe"
    Copy-Item target\release\Spotlight.exe $portable

    $iscc = @("$env:LOCALAPPDATA\Programs\Inno Setup 6\ISCC.exe", "${env:ProgramFiles(x86)}\Inno Setup 6\ISCC.exe", "$env:ProgramFiles\Inno Setup 6\ISCC.exe") |
        Where-Object { Test-Path $_ } | Select-Object -First 1
    if (-not $iscc) { throw "Inno Setup 6 isn't installed: winget install JRSoftware.InnoSetup" }
    & $iscc /Q "/DAppVersion=$version" "/DSourceExe=$root\target\release\Spotlight.exe" installer\spotlight.iss
    if ($LASTEXITCODE -ne 0) { throw "installer build failed" }

    Write-Host "wrote $(Join-Path $dist "Spotlight-$version-Setup.exe")"
    Write-Host "wrote $portable"
} finally { Pop-Location }
