# Getting started

## Requirements

- Rust 1.87+ (stable). Tested with Rust 1.96.
- macOS, Linux, or Windows. There is no SDL2 dependency: video uses
  `winit` + `pixels`, audio uses `cpal`, and gamepads use the pure-Rust
  `gilrs` crate.
- Fedora build dependencies: `sudo dnf install alsa-lib-devel systemd-devel`.
- A boot ROM. Copperline ships with the [AROS](https://www.aros.org/)
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
~7.09 MHz with 512 KiB chip RAM, OCS, PAL, and the bundled AROS ROM (when no
ROM is named, Copperline locates the AROS image that ships with it -- see
[](configuration#top-level)). Because AROS needs more than a bare 512 KiB
A500, the default machine is fitted with 512 KiB of trapdoor (slow) RAM in
this case -- a stock 1 MB A500 -- so it boots to the AROS "waiting for
bootable media" screen. Naming your own ROM, machine, or memory turns this
off.

You can boot your own ROM with a positional argument, or point at a specific
config file:

```sh
./target/release/copperline path/to/kickstart.rom
./target/release/copperline --config path/to/copperline.toml
```

The common machine knobs can also be set straight on the command line,
without writing a config file at all -- the machine model, chipset, CPU
(and its clock/FPU), and the chip/fast RAM sizes:

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
