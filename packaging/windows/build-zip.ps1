#!/usr/bin/env pwsh
# Build a Copperline Windows release zip: a portable, no-install bundle that
# runs without administrator rights. Run on a Windows host (or CI); see
# .github/workflows/windows.yml.
#
# What it does:
#   1. Builds the release binary for x86_64-pc-windows-msvc with the pinned
#      dependency graph. The CRT is statically linked (see .cargo/config.toml),
#      so the bundle needs no Visual C++ Redistributable.
#   2. Stages a folder holding copperline.exe with a sibling aros\ directory,
#      which is the first location romsearch.rs probes, so the bundled AROS
#      ROM is found with no configuration.
#   3. Zips the folder into Copperline-<version>-win-x64.zip, mirroring the
#      AppImage/Homebrew version naming so release assets are self-describing.
$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest

$here = Split-Path -Parent $MyInvocation.MyCommand.Path
$repoRoot = (Resolve-Path (Join-Path $here "..\..")).Path
Set-Location $repoRoot

$target = "x86_64-pc-windows-msvc"

# Version from Cargo.toml, matching the AppImage/Homebrew naming convention.
$version = (Select-String -Path "Cargo.toml" -Pattern '^version\s*=\s*"([^"]+)"').Matches[0].Groups[1].Value
$stageName = "Copperline-$version-win-x64"
$stage = Join-Path $repoRoot $stageName
$zipPath = Join-Path $repoRoot "$stageName.zip"

Write-Host "==> Building release binary ($target)"
cargo build --release --locked --target $target
if ($LASTEXITCODE -ne 0) { throw "cargo build failed" }

Write-Host "==> Staging $stageName"
if (Test-Path $stage) { Remove-Item -Recurse -Force $stage }
$arosDir = Join-Path $stage "aros"
New-Item -ItemType Directory -Force -Path $arosDir | Out-Null

Copy-Item "target\$target\release\copperline.exe" (Join-Path $stage "copperline.exe")

# Bundled AROS open-source Kickstart replacement (the default boot ROM).
# romsearch.rs probes a sibling aros\ next to the executable first. Ship the
# license/readme/acknowledgements next to the ROM halves as redistribution
# requires.
foreach ($f in @(
    "aros-amiga-m68k-rom.bin",
    "aros-amiga-m68k-ext.bin",
    "LICENSE",
    "README.md",
    "ACKNOWLEDGEMENTS")) {
    Copy-Item "assets\aros\$f" (Join-Path $arosDir $f)
}

# Top-level docs and an example config to get users started.
Copy-Item "copperline.example.toml" $stage
Copy-Item "LICENSE" (Join-Path $stage "LICENSE.txt")
Copy-Item "packaging\windows\README.txt" (Join-Path $stage "README.txt")

Write-Host "==> Zipping $zipPath"
if (Test-Path $zipPath) { Remove-Item -Force $zipPath }
Compress-Archive -Path $stage -DestinationPath $zipPath -CompressionLevel Optimal

Write-Host "==> Built $stageName.zip"
