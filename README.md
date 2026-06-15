![Copperline](assets/brand/copperline-logo.png)

Website: [copperline.dev](https://copperline.dev/)

An Amiga emulator written in Rust, built around a vendored copy of
the pure-Rust [m68k](vendor/m68k) CPU core, with a
[pixels](https://crates.io/crates/pixels) + [winit](https://crates.io/crates/winit)
window for video and stdout for serial. It started life with the modest
goal of booting [DiagROM](https://www.diagrom.com/) far enough to show a
menu; it now boots Kickstart and runs timing-sensitive OCS and AGA software
from the regression set at real speed.

It covers OCS, ECS, and AGA (independent Agnus/Denise revisions,
programmable blanking, machine profiles from the A500 to the A1200,
CDTV, and CD32, with Gayle IDE, A2091 SCSI, and the AGA display path: 8 bitplanes,
256-entry palette, HAM8, FMODE wide fetch; remaining gaps are recorded in
the internals docs). The timing model is taken seriously: the
chip bus is arbitrated per colour clock, the Copper and blitter are
scheduled per DMA slot with hardware bus sequences, and 68000
interrupt-recognition latency is modelled. The source is organized so the
hardware model can be read and extended without chasing title-specific
patches.

## Background

It began as a two-part experiment: could I build an Amiga emulator that is
easy to drive from the command line, and could I give it the tooling to
feed debugging data and screenshots back to an AI agent automatically, so
issues could be investigated from a timestamp or a snapshot? It grew into a
larger emulator whose development was AI-driven, guided by books, my own
knowledge of Amiga internals, and test disks generated to compare timings
against real hardware.

## Features

- **Cycle-driven timing core.** The chip bus is arbitrated per colour clock
  between refresh, display/sprite/disk/audio DMA, the Copper, the blitter,
  and CPU accesses; the Copper and blitter are scheduled per DMA slot with
  the hardware per-word bus sequences, and 68000 interrupt-recognition
  latency is modelled. Real-hardware reference numbers come from the
  cross-emulator disk in `timing-test/`.
- **OCS, ECS, and AGA**, with independent Agnus/Denise revisions and machine
  profiles from the A500 to the A1200, plus CDTV and CD32. Boots the bundled
  AROS ROM out of the box, as well as Kickstart 1.3 / 2.05 / 3.1 and DiagROM
  v2.0, and runs the current timing-sensitive OCS and AGA regression set at
  real speed.
- **Configurable CPU** (68000 / 68EC020 / 68020 / 68030 / 68040) and clock,
  with an optional 68881/68882 FPU (default-on for the 68040).
- **Peripherals**: a bit-timed keyboard (6500/1 MCU), mouse, USB gamepad
  (via the pure-Rust `gilrs`, no SDL2), 4-channel Paula audio, floppy
  (ADF / ADZ / DMS, read-only SCP), Gayle IDE, A2091 SCSI, and CDTV/CD32 CD.
- **Tooling**: an in-window debugger, deterministic save states, input
  recording/replay, and headless screenshot/frame-dump capture -- the
  deterministic core makes every replay byte-identical.

## Requirements

- Rust 1.87+ (stable). Tested with Rust 1.96.
- Fedora build dependencies: `sudo dnf install alsa-lib-devel systemd-devel`.
- No SDL2 dependency. Developed and tested on **macOS**; the Linux and
  Windows paths are expected to work but are currently untested.

## Install (macOS, Homebrew)

```sh
brew tap LinuxJedi/copperline https://github.com/LinuxJedi/Copperline
brew install copperline
```

This builds from source on your machine, so the binary is not subject to
macOS Gatekeeper quarantine -- there is no Security & Privacy override to
click through. Use `brew install --HEAD copperline` to build the latest
`main` instead of the most recent tagged release. Then run `copperline` from
the terminal.

## Build and run

```sh
cargo build --release
./target/release/copperline
```

The binary looks for `./copperline.toml`; if it isn't present, built-in
defaults are used (68000 at ~7.09 MHz, OCS, PAL, real speed, and the bundled
[AROS](https://www.aros.org/) ROM on a 1 MB A500). Boot your own ROM with a
positional argument, or point at a config file:

```sh
./target/release/copperline path/to/kickstart.rom
./target/release/copperline --config path/to/copperline.toml
```

Essential shortcuts use `Cmd` on macOS and `Alt` on Linux/Windows:
`Cmd+Q` / `Alt+Q` closes the window, `Esc` passes through to the Amiga (or
closes an open menu/window), `Cmd+S` / `Alt+S` saves a screenshot, and
`Cmd+B` / `Alt+B` opens the debugger. The status bar, pop-up menu, overlay
windows (debugger, gamepad calibration, save/load state, input recording),
and the full shortcut list are documented in the
[user guide](docs/guide/ui.md).

## Configuration

Copy `copperline.example.toml` to `copperline.toml` and edit; every field is
optional and missing fields use documented defaults.

```toml
rom = "kickstart205.rom"

[cpu]
model = "68000"       # 68000, 68EC020, 68020, 68030, 68040
# fpu = true          # fit a 68881/68882 (default-on for the 68040)
# clock_mhz = 14.0    # defaults to the model's stock speed

[memory]
chip = "512K"         # OCS 512K, ECS/AGA up to 2M
fast = "0"            # Zorro II autoconfig fast RAM, up to 8M
slow = "0"            # A500 trapdoor RAM at $C00000, up to 512K

[chipset]
revision = "OCS"      # OCS, ECS, or AGA (picks the Agnus/Denise revisions)
video = "PAL"         # PAL or NTSC

[floppy.df0]
path = "AmigaTestKit.adf"   # DD ADF / ADZ / DMS / SCP; omit for no disk
```

The full reference -- every key, machine profiles, Zorro boards, CD/HDD
images, validation rules, and audio options -- is in the
[configuration guide](docs/guide/configuration.md).

## Documentation

User and developer documentation -- getting started, the UI and shortcuts,
headless capture, save states, input recording, both debuggers, the
configuration reference, and the internals (timing model, chipset, CPU,
video pipeline) -- is published at [copperline.dev](https://copperline.dev/)
and lives under `docs/` as a [MyST](https://mystmd.org/) project you can also
build locally:

```sh
npm install -g mystmd
cd docs && myst build --html      # static site in docs/_build/html
```

See `docs/README.md` for conventions and PDF output.

## Packaging

Copperline is intended to be distributed from source for now. `Cargo.toml`
sets `publish = false` because the emulator depends on a patched vendored
copy of the `m68k` CPU core; a crates.io release needs that dependency
resolved first.

## What gets emulated

| Subsystem | Notes |
| --- | --- |
| M68K CPU | Via a vendored pure-Rust m68k crate; model selectable through 68040, accurate 68000 cycle counts, 6888x FPU. |
| Chip RAM | mem_map'd; reset starts with ROM overlaid at $0 until CIA-A releases /OVL. |
| Fast RAM | Optional Zorro II autoconfig RAM at $00200000 and Zorro III autoconfig RAM (`[memory] z3`); runs at the CPU clock. |
| Slow RAM | Optional A500 trapdoor/fake-fast RAM at $00C00000; arbitrated on the chip bus through Agnus like chip RAM. |
| ROM | Kickstart at $F80000 (512 KiB); optional extended ROM for CD32 ($E00000) and CDTV ($F00000). |
| Battery RTC | Read-only MSM6242-compatible register view at $DC0000; guest writes affect only emulated latch/control state. |
| CIA-A / CIA-B | I/O ports, /OVL, timers, TOD, keyboard SDR/ICR, disk control/status lines, and CIA-B FLAG disk index pulses. |
| Paula serial | SERDAT -> stdout through a one-word transmit buffer and timed shift register; SERDATR reports TBE/TSRE/RBF. |
| Paula audio | 4-channel DMA/sample playback, stereo mix, LED filter. |
| Paula DMACON / INTENA / INTREQ | IRQ bits are stored and delivered through manual M68K autovectors with modelled 68000 interrupt-recognition latency; audio and disk DMA raise completion IRQs. |
| Floppy / ADF / DMS / SCP | DF0-DF3 standard DD ADF read/write, read-only ADZ/DMS, UAE extended ADF, initial read-only SCP flux import, track-timed disk DMA, CIA drive lines, index FLAG, DSKLEN/DSKBYTR/DSKSYNC/DSKDAT, per-drive multi-disk playlists with a swap key. |
| Agnus VPOSR / VHPOSR | Beam counters advanced per colour clock; PAL and NTSC timing (including NTSC long/short lines). |
| Agnus Copper | Beam-scheduled OCS Copper with COP1/COP2 jumps, WAIT, SKIP, DMAEN/COPEN gating, and chip-bus grants. |
| Agnus blitter | Scheduled per-slot engine: normal/line/fill modes, hardware per-word channel bus sequences (including the area-fill idle C slot), BBUSY/BZERO, BLTPRI "nasty" vs CPU starvation-yield arbitration, blit-done IRQ. |
| Denise BPLCON / COLORxx | Stored and replayed by beam position. |
| Bitplane renderer | OCS lo-res or hi-res; reads chip RAM via BPLxPT; honours modulos and beam-timed BPLCON1 scroll; EHB, HAM, dual playfield, and CLXDAT collisions. Lo-res pixel-doubles horizontally to match the 716-wide framebuffer. |
| Display window | winit 0.30 + pixels 0.17 surface; 716x285 framebuffer presented at 4:3 (716x537) plus a 44-pixel status bar with power/disk controls. |
| Keyboard / mouse / gamepad | Host keyboard/mouse mapped to Amiga input paths; key down and key up events go through CIA-A SDR/ICR with acknowledge + KDAT handshake backpressure and keyboard-MCU pacing; mouse deltas feed JOY0DAT; `Cmd+G` on macOS or `Alt+G` on Linux/Windows toggles host mouse capture; a USB gamepad (gilrs) drives the port-2 digital joystick (JOY1DAT directions, /FIR1 fire, POT1Y button 2); Ctrl+Ami+Ami resets. |
| OCS sprites | 8 DMA/manual 16-pixel sprites, attached sprites, composited over bitplanes with playfield priority. |
| Chip bus arbitration | Per-colour-clock OCS slot ownership for refresh, display DMA, sprites, disk, audio, Copper, blitter, and CPU chip/custom accesses, with CPU wait states. |
| ECS | ECS Agnus revisions (8372A/8375) and ECS Denise (8373): up to 2M chip RAM, DIWHIGH, BEAMCON0, SuperHires, ECS blitter (BLTSIZV/BLTSIZH), programmable geometry. |
| AGA | Alice/Lisa: 8 bitplanes, 256-entry 25-bit palette with BANK/LOCT, HAM8, FMODE wide bitplane/sprite fetch, BPLCON4, CLXCON2; A1200/CD32 profiles. Remaining gaps (e.g. 35 ns SHRES sprites) recorded in the internals docs. |

The detailed architecture (source layout, the bus, the replay renderer) and
timing model live in the [internals docs](docs/internals/architecture.md).

## Tests

`cargo test` runs the unit suite, which needs no external assets. The
integration tests under `tests/` are marked `#[ignore]` because they run the
emulator against local Kickstart ROM and disk images that are not part of
the repository; they skip cleanly when the assets are absent. See
[`tests/README.md`](tests/README.md) for the asset list and lookup
directory, and the [headless guide](docs/guide/headless.md) for the vAmigaTS
compatibility runs. `timing-test/` is a bootable disk that measures CPU and
chip-bus timings against the CIA E-clock for cross-emulator comparison.

## Known quirks

- Lo-res content is pixel-doubled horizontally inside the framebuffer; the
  window presents the field at a TV-like 4:3 aspect plus the status bar.
- The CPU may halt with an `EXCEPTION` on an unimplemented feature (an exotic
  custom register or CIA edge case). This is non-fatal: the window stays
  alive showing the last framebuffer, and the debugger can inspect the
  halted state.

## Credits

- The [AROS Research Operating System](https://www.aros.org/), bundled in
  [`assets/aros`](assets/aros) as the default boot ROM. AROS is an
  open-source re-implementation of the AmigaOS API, distributed under the
  AROS Public License; the ROM images here are unmodified from the official
  m68k nightly build (see [assets/aros/README.md](assets/aros/README.md)).
- [DiagROM](https://www.diagrom.com/) by John "Chucky" Hertell,
  licensed for free use.
- The [m68k](vendor/m68k) CPU core vendored under `vendor/m68k`.
- The public-domain `font8x8` glyphs by Daniel Hepper / Marcel Sondaar
  for the on-screen overlay font.
- The Amiga Hardware Reference Manual for register-level documentation.

## License

Copperline is free software, released under the GNU General Public
License version 3 or (at your option) any later version. See
[LICENSE](LICENSE) for the full text. The vendored
[m68k](vendor/m68k) CPU core under `vendor/m68k`
retains its own MIT license.

## Trademarks

Amiga and Commodore are trademarks of their respective owners. Copperline
is an independent, unofficial project and is not affiliated with, sponsored
by, or endorsed by any trademark holder.
