#!/usr/bin/env bash
# Build a Copperline macOS disk image: a drag-to-Applications Copperline.app
# wrapped in a .dmg. Run from a macOS host (or CI); see
# .github/workflows/macos.yml.
#
# What it does:
#   1. Builds the release binary for both Apple architectures with the pinned
#      dependency graph and lipo-joins them into one universal binary, so a
#      single download runs natively on Apple Silicon and Intel.
#   2. Stages a Copperline.app bundle: the universal binary in Contents/MacOS,
#      the icon and AROS ROM in Contents/Resources. romsearch.rs probes a
#      bundle's Contents/Resources/aros first, so the bundled AROS ROM is found
#      with no configuration.
#   3. Ad-hoc code-signs the bundle. lipo strips the per-slice signatures Rust
#      attaches on macOS, and an unsigned arm64 binary will not launch on Apple
#      Silicon at all, so a signature is mandatory even when it is ad-hoc.
#   4. Lays out a .dmg with the app and an Applications symlink, named
#      Copperline-<version>-macos-universal.dmg to mirror the
#      AppImage/Windows/Homebrew version naming so release assets are
#      self-describing.
#
# This build is intentionally NOT signed with a Developer ID or notarized, so
# first launch trips Gatekeeper; packaging/macos/README.txt (shipped in the
# image) explains the right-click-Open workaround.
#
# Override knobs (env):
#   MACOS_UNIVERSAL=0   build only the host architecture (faster local builds)
#   OUTPUT=<path>       final .dmg file name
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd "$here/../.." && pwd)"
cd "$repo_root"

version="$(grep -m1 '^version' Cargo.toml | cut -d'"' -f2)"
app_name="Copperline.app"
stage="$repo_root/target/macos-dmg"
app="$stage/$app_name"
output="${OUTPUT:-$repo_root/Copperline-$version-macos-universal.dmg}"

# Architectures to build. Default is universal (both); MACOS_UNIVERSAL=0 builds
# only the host arch for a faster local turnaround.
if [ "${MACOS_UNIVERSAL:-1}" = "0" ]; then
  case "$(uname -m)" in
    arm64) targets=(aarch64-apple-darwin) ;;
    *) targets=(x86_64-apple-darwin) ;;
  esac
else
  targets=(aarch64-apple-darwin x86_64-apple-darwin)
fi

echo "==> Building release binary (${targets[*]})"
for target in "${targets[@]}"; do
  # Idempotent; ensures hand-builds on a fresh checkout have the cross target.
  if command -v rustup >/dev/null 2>&1; then
    rustup target add "$target" >/dev/null
  fi
  cargo build --release --locked --target "$target"
done

echo "==> Staging $app_name"
rm -rf "$stage"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources/aros"

# Universal binary from the per-arch builds (a single-arch lipo is a no-op copy).
bins=()
for target in "${targets[@]}"; do
  bins+=("target/$target/release/copperline")
done
lipo -create -output "$app/Contents/MacOS/copperline" "${bins[@]}"

# Info.plist with the version substituted in; plutil -lint catches a botched
# substitution before the bundle ships.
sed "s/@VERSION@/$version/g" "$here/Info.plist.in" > "$app/Contents/Info.plist"
plutil -lint "$app/Contents/Info.plist" >/dev/null

# Classic four-character package signature; harmless but expected by some tools.
printf 'APPL????' > "$app/Contents/PkgInfo"

cp "assets/brand/copperline.icns" "$app/Contents/Resources/copperline.icns"

# Bundled AROS open-source Kickstart replacement (the default boot ROM).
# romsearch.rs resolves Contents/Resources/aros relative to the executable.
# Ship the license/readme/acknowledgements next to the ROM halves as
# redistribution requires.
for f in \
  aros-amiga-m68k-rom.bin \
  aros-amiga-m68k-ext.bin \
  LICENSE \
  README.md \
  ACKNOWLEDGEMENTS; do
  cp "assets/aros/$f" "$app/Contents/Resources/aros/$f"
done

echo "==> Ad-hoc signing $app_name"
# --deep so the nested executable is signed too; "-" selects the ad-hoc
# identity (no Developer ID needed). This is what lets the universal binary
# launch on Apple Silicon; it does not satisfy notarization, so downloads are
# still Gatekeeper-quarantined (see README.txt).
codesign --force --deep --sign - "$app"

echo "==> Laying out disk image contents"
# Top-level docs alongside the app, mirroring the Windows zip: an Applications
# shortcut for drag-installs, the README's Gatekeeper note, a starter config,
# and the Copperline license that must accompany the binary.
ln -s /Applications "$stage/Applications"
cp "$here/README.txt" "$stage/README.txt"
cp "copperline.example.toml" "$stage/copperline.example.toml"
cp "LICENSE" "$stage/LICENSE.txt"

echo "==> Building $(basename "$output")"
rm -f "$output"
hdiutil create \
  -volname "Copperline $version" \
  -srcfolder "$stage" \
  -fs HFS+ \
  -format UDZO \
  -ov \
  "$output" >/dev/null

echo "==> Built $output"
