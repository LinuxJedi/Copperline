# Getting started

## Requirements

- Rust 1.87+ (stable). Tested with Rust 1.96.
- macOS, Linux, or Windows. There is no SDL2 dependency: video uses
  `winit` + `pixels`, audio uses `cpal`, and gamepads use the pure-Rust
  `gilrs` crate.
- A GPU backend for presentation: Metal on macOS, DX12 on Windows, and
  **Vulkan on Linux** (see [](#vulkan-is-required-on-linux)).
- Fedora build dependencies: `sudo dnf install alsa-lib-devel systemd-devel`.
- A boot ROM. Copperline ships with the [AROS](http://www.aros.org/)
  open-source Kickstart replacement and boots it by default, so it runs out
  of the box with no ROM of your own. It also boots Kickstart 1.3, 2.05, and
  3.1 (including the CDTV and CD32 extended ROMs) as well as
  [DiagROM](https://www.diagrom.com/). Main Kickstart images must be exactly
  512 KiB; CDTV/CD32 extended ROM sizes are covered in
  [](configuration#top-level).

## Installing on macOS (Homebrew)

```sh
brew tap LinuxJedi/copperline https://github.com/LinuxJedi/Copperline
brew install copperline
```

The formula builds from source, so the binary is compiled locally and is not
subject to macOS Gatekeeper quarantine: there is no Security & Privacy
override to click through, unlike a downloaded prebuilt app. Use
`brew install --HEAD copperline` to build the latest `main` instead of the
most recent tagged release, then run `copperline` from the terminal.

For a no-compiler install, download `Copperline-X.Y.Z-macos-universal.dmg`
from the [releases page](https://github.com/LinuxJedi/Copperline/releases),
open it, and drag `Copperline.app` onto the Applications shortcut. The app is a
universal binary that runs natively on Apple Silicon and Intel, and bundles the
AROS boot ROM, so it runs out of the box. The image is not code-signed or
notarized, so on first launch Gatekeeper refuses to open it; right-click (or
Control-click) the app and choose **Open**, then confirm. macOS remembers the
choice. If it still refuses, clear the download quarantine with
`xattr -dr com.apple.quarantine /Applications/Copperline.app`.

## Installing on Linux

Two channels are provided.

**Flatpak** (recommended) works on any distribution and pulls in the GPU,
audio and portal stack from the Freedesktop runtime, so there is nothing else
to install:

```sh
flatpak install flathub dev.copperline.Copperline
flatpak run dev.copperline.Copperline
```

**AppImage** is a single self-contained file that needs no installation:
download `Copperline-X.Y.Z-<arch>.AppImage` from the
[releases page](https://github.com/LinuxJedi/Copperline/releases), then:

```sh
chmod +x Copperline-*.AppImage
./Copperline-*.AppImage
```

Both bundle the AROS boot ROM, so they run out of the box. Packaging sources
live in `packaging/flatpak/` and `packaging/appimage/`.

### Vulkan is required on Linux

The display is presented through wgpu's Vulkan backend. The OpenGL fallback
is disabled because wgpu initializes its EGL instance without a display
handle and silently selects Mesa's "surfaceless" platform, which cannot be
paired with an on-screen window; adapter selection then fails. The symptom is
the window flashing open and immediately exiting with:

```
ERROR copperline::video::window] pixels init failed: No suitable `wgpu::Adapter` found.
```

The fix is to provide a Vulkan driver. Any GPU from roughly Intel Skylake /
2015 onward ships a hardware Vulkan driver in `mesa`. Older hardware, a
headless host, or a VM can use the software lavapipe ICD instead:

- Arch: `sudo pacman -S vulkan-swrast`
- Debian/Ubuntu: `sudo apt install mesa-vulkan-drivers`
- Fedora: `sudo dnf install mesa-vulkan-drivers`

Copperline renders entirely on the CPU and only asks the GPU to blit one
framebuffer per frame, so software Vulkan is perfectly adequate. The Flatpak
runtime already includes lavapipe, so the Flatpak needs no extra package.
`WGPU_BACKEND` overrides backend selection if you need to force one for
debugging.

## Building

```sh
cargo build --release
```

```{warning}
Always use a release build to run software. Debug builds are far too slow
for real-time emulation.
```

The test suite needs no external assets:

```sh
cargo test                          # asset-free test suite
cargo test --release -- --ignored   # integration tests (need local ROMs/disks)
```

## First boot

```sh
./target/release/copperline
```

With no arguments, Copperline looks for `./copperline.toml` in the current
directory. If it is not present, built-in defaults are used: a 68000 at
~7.09 MHz with 512 KiB chip RAM, PAL, and the bundled AROS ROM (when no
ROM is named, Copperline locates the AROS image that ships with it -- see
[](configuration#top-level)). The default machine is the A500 Rev 6A -- the
most common and most-targeted Amiga: the ECS "Fatter" 8372A Agnus (1 MiB
chip reach plus the software PAL/NTSC switch) with the original OCS 8362
Denise, 512 KiB chip RAM plus 512 KiB of trapdoor slow RAM. Use `--slow 0`
or `[memory] slow = "0"` for a bare 512 KiB machine, or `[chipset] revision
= "OCS"` for a plain 8371/8362 OCS A500.

You can boot your own ROM with a positional argument, or point at a specific
config file:

```sh
./target/release/copperline path/to/kickstart.rom
./target/release/copperline --config path/to/copperline.toml
```

The common machine knobs can also be set straight on the command line,
without writing a config file at all -- the machine model, chipset, CPU
(and its clock/FPU), and the chip/fast/slow RAM sizes:

```sh
./target/release/copperline --model A1200 --fast 8M KICK31.ROM
```

See [](configuration#command-line-overrides) for the full list.

A Kickstart 1.3 machine with no disk boots to the familiar insert-disk
screen:

```{figure} ../images/kick13-insert-disk.png
:alt: Kickstart 1.3 insert-disk screen
:width: 75%

Kickstart 1.3 waiting for a boot floppy.
```

To boot a disk, add a floppy section to your config:

```toml
rom = "KICK13.ROM"

[floppy.df0]
path = "MyGame.adf"
```

Copperline accepts plain ADF images, gzip-compressed images, DMS archives,
UAE extended ADFs, and read-only SCP flux images.

## Example configuration

`copperline.example.toml` in the repository root is a commented reference
covering every option -- machine profiles, CPU/FPU, memory,
chipset, floppy/HDD/CD images, and audio. Copy it to `copperline.toml`
(or pass it with `--config`) and edit; it doubles as a worked example for
the options described in [](configuration).

```sh
./target/release/copperline --config copperline.example.toml
```

## Logging

Copperline logs through the standard Rust `log`/`env_logger` machinery.
`RUST_LOG=debug` (or `trace`) prints more detail from the CPU and MMIO
layers, and is also how the [headless debugger](../debugger/headless)
output is surfaced.
