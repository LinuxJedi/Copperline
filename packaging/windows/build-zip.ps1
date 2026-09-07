#!/usr/bin/env pwsh
# Build a Copperline Windows release zip: a portable, no-install bundle that
# runs without administrator rights. Run on a Windows host (or CI); see
# .github/workflows/windows.yml.
#
# What it does:
#   1. Builds the release binary for the requested MSVC target (x86-64 by
#      default, ARM64 with -Target aarch64-pc-windows-msvc) with the pinned
#      dependency graph. The CRT is statically linked (see .cargo/config.toml),
#      so the bundle needs no Visual C++ Redistributable.
#   2. Stages a folder holding copperline.exe with a sibling aros\ directory,
#      which is the first location romsearch.rs probes, so the bundled AROS
#      ROM is found with no configuration; the other bundled ROM assets
#      (fmv\, a4091\, a2091\, lide\, hrtmon\) sit beside it the same way.
#   3. Zips the folder into Copperline-<version>-win-<x64|arm64>.zip,
#      mirroring the AppImage/Homebrew version naming so release assets are
#      self-describing.
param(
    [string]$Target = "x86_64-pc-windows-msvc"
)
$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest

$here = Split-Path -Parent $MyInvocation.MyCommand.Path
$repoRoot = (Resolve-Path (Join-Path $here "..\..")).Path
Set-Location $repoRoot

$target = $Target
$arch = switch ($target) {
    "x86_64-pc-windows-msvc" { "x64" }
    "aarch64-pc-windows-msvc" { "arm64" }
    default { throw "unsupported target $target" }
}

# Version from Cargo.toml, matching the AppImage/Homebrew naming convention.
$version = (Select-String -Path "Cargo.toml" -Pattern '^version\s*=\s*"([^"]+)"').Matches[0].Groups[1].Value
$stageName = "Copperline-$version-win-$arch"
$stage = Join-Path $repoRoot $stageName
$zipPath = Join-Path $repoRoot "$stageName.zip"

& (Join-Path $here "enable-clang.ps1") -Target $target

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

# Bundled open CD32 FMV cartridge ROM (the CD32 profile default).
$fmvDir = Join-Path $stage "fmv"
New-Item -ItemType Directory -Force -Path $fmvDir | Out-Null
foreach ($f in @("copperline-fmv.rom", "README.md")) {
    Copy-Item (Join-Path "assets\fmv" $f) (Join-Path $fmvDir $f)
}

# Bundled open-source A4091 autoboot ROM (default when a config fits an A4091
# without naming a ROM); romsearch.rs probes a sibling a4091\ next to the exe.
$a4091Dir = Join-Path $stage "a4091"
New-Item -ItemType Directory -Force -Path $a4091Dir | Out-Null
foreach ($f in @("a4091_cdfs.rom", "README.md", "THIRD_PARTY_NOTICES.txt")) {
    Copy-Item (Join-Path "assets\a4091" $f) (Join-Path $a4091Dir $f)
}

# Copperline's open A2091/A590 autoboot ROM.
$a2091Dir = Join-Path $stage "a2091"
New-Item -ItemType Directory -Force -Path $a2091Dir | Out-Null
foreach ($f in @("copperline-a2091.rom", "README.md", "THIRD_PARTY_NOTICES.txt")) {
    Copy-Item (Join-Path "assets\a2091" $f) (Join-Path $a2091Dir $f)
}

# Bundled open-source lide.device autoboot ROM and CD-filesystem bank
# (default for a fitted [lide] board without a named rom/rom_bank2);
# romsearch.rs probes a sibling lide\ next to the exe.
$lideDir = Join-Path $stage "lide"
New-Item -ItemType Directory -Force -Path $lideDir | Out-Null
foreach ($f in @("lide.rom", "lide-atbus.rom", "cdfs.rom", "README.md", "THIRD_PARTY_NOTICES.txt")) {
    Copy-Item (Join-Path "assets\lide" $f) (Join-Path $lideDir $f)
}

# Bundled HRTMon freezer-cartridge image (default for [cartridge] model =
# "hrtmon" / --cartridge hrtmon without a named rom); romsearch.rs probes a
# sibling hrtmon\ next to the exe. GPL-2.0-or-later: ship its notice and
# license.
$hrtmonDir = Join-Path $stage "hrtmon"
New-Item -ItemType Directory -Force -Path $hrtmonDir | Out-Null
foreach ($f in @("hrtmon.rom", "README.md", "LICENSE")) {
    Copy-Item (Join-Path "assets\hrtmon" $f) (Join-Path $hrtmonDir $f)
}

# WHDLoad support archives (direct WHDLoad boot, src/whdload.rs); fetched
# with checksums pinned in step with tools/fetch-whdload.sh (the sh script
# does not run on Windows runners) and shipped unmodified next to the exe,
# where whdload::find_whdboot_assets probes a sibling whdboot\ directory.
$whdbootSources = @(
    @{ Url = "https://whdload.de/whdload/WHDLoad_usr.lha"
       Sha256 = "093333953737528d79c1eda7d21a16a0aa298698722624e7cfb31f588a0a156d" },
    @{ Url = "https://aminet.net/util/boot/skick346.lha"
       Sha256 = "02b4d01852d12ab391c6469064f917221a0f7319fd0b3ba6c359403ec1d59f96" }
)
$whdbootAssets = "assets\whdboot"
foreach ($src in $whdbootSources) {
    $file = Join-Path $whdbootAssets (Split-Path $src.Url -Leaf)
    if (-not (Test-Path $file) -or
        (Get-FileHash $file -Algorithm SHA256).Hash.ToLower() -ne $src.Sha256) {
        Write-Host "==> Fetching $($src.Url)"
        Invoke-WebRequest -Uri $src.Url -OutFile $file
        $got = (Get-FileHash $file -Algorithm SHA256).Hash.ToLower()
        if ($got -ne $src.Sha256) {
            throw "checksum mismatch for $($src.Url): expected $($src.Sha256), got $got"
        }
    }
}
$whdbootDir = Join-Path $stage "whdboot"
New-Item -ItemType Directory -Force -Path $whdbootDir | Out-Null
foreach ($f in @("WHDLoad_usr.lha", "skick346.lha", "README.md")) {
    Copy-Item (Join-Path $whdbootAssets $f) (Join-Path $whdbootDir $f)
}

# Top-level docs and an example config to get users started.
Copy-Item "copperline.example.toml" $stage
Copy-Item "LICENSE" (Join-Path $stage "LICENSE.txt")
Copy-Item "packaging\windows\README.txt" (Join-Path $stage "README.txt")

Write-Host "==> Zipping $zipPath"
if (Test-Path $zipPath) { Remove-Item -Force $zipPath }
Compress-Archive -Path $stage -DestinationPath $zipPath -CompressionLevel Optimal

Write-Host "==> Built $stageName.zip"
