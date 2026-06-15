# Architecture overview

This chapter is the map; the following chapters zoom into the
[timing model](timing), the [chipset modules](chipset), the
[video pipeline](video), the [CPU integration](cpu), the
[peripherals](peripherals), and the [save-state format](savestate).

## Source layout

```
src/
  main.rs           # arg parse, config load, wire everything
  config.rs         # TOML config + validation + machine profiles
  envcfg.rs         # cached COPPERLINE_* environment-variable snapshot
  emulator.rs       # frame loop driving CPU, chipset, and host I/O
  debugger.rs       # env-driven headless debugger
  disasm.rs         # 68000 + Copper-list disassemblers
  cpu.rs            # m68k core wrapper and CPU-visible bus adapter
  bus.rs            # shared RAM, ROM, chipset, CIA, RTC, and I/O state
  memory.rs         # chip/slow RAM, ROM, extended ROM containers
  zorro.rs          # Zorro II/III autoconfig chain and boards
  floppy.rs         # disk images + timed disk DMA controller
  dms.rs            # DMS archive decompression
  gayle.rs          # A600/A1200 Gayle gate array + IDE
  a2091.rs          # A2091 SCSI controller board (DMAC + boot ROM)
  scsi.rs           # WD33C93A SBIC + SCSI-2 disk targets
  harddrive.rs      # shared hard-drive image backend (IDE + SCSI)
  cdrom.rs          # CD image (BIN/CUE) parsing
  cdtv.rs           # CDTV DMAC + Matshita drive model
  akiko.rs          # CD32 Akiko (C2P, NVRAM, Chinon drive)
  rtc.rs            # MSM6242-compatible battery RTC
  serial.rs         # Paula serial sink (stdout)
  audio.rs          # AudioSink trait + cpal/WAV/null outputs
  gamepad.rs        # gilrs input + guided calibration
  screenshot.rs     # PNG export helpers
  recorder.rs       # video+audio capture (ZMBV/PCM AVI writer)
  inputrec.rs       # live-input recording to the scripted-input format
  savestate.rs      # whole-machine snapshot/restore (versioned file format)
  chipset/
    agnus.rs        # beam counters, DMACON, display fetch, arbitration data
    copper.rs       # Copper decode + cycle-stepped execution
    blitter.rs      # scheduled per-DMA-slot blitter engine
    paula.rs        # interrupts, audio DMA, serial, disk regs
    denise.rs       # palette + bitplane/sprite control registers
    cia.rs          # 8520 CIA model (CIA-A and CIA-B)
  video/
    beam.rs         # beam-position event index for the renderer
    bitplane.rs     # event replay + planar->RGBA renderer
    deinterlace.rs  # motion-adaptive deinterlacer
    window.rs       # winit ApplicationHandler + pixels surface + status bar
    ui.rs           # pop-up menu + overlay windows (debugger etc.)
    font.rs         # 8x8 overlay font
vendor/m68k/        # vendored m68k CPU core
tests/              # ignored integration tests (need local ROM assets)
timing-test/        # bootable cross-emulator timing-measurement disk
```

## The big picture

Copperline runs synchronously on the main thread inside winit's event
loop. There is no emulation thread and no locking between CPU and chipset:
each turn of the loop (`Emulator::step_real`, `src/emulator.rs`) advances
the deterministic core a frame's worth of emulated time, cycle-stepping the
CPU and the chipset together, then presentation paints from what the core
recorded.

The flow of a frame:

1. The frame loop hands the CPU an instruction budget. The CPU executes
   one instruction at a time through the vendored m68k core; every memory
   access the instruction makes is routed through the bus adapter and
   *billed in colour clocks* (CCK, 3.546895 MHz -- the chip bus clock).
2. Advancing the clock for a CPU access also advances everything else:
   Agnus beam counters, Copper fetches, blitter slots, Paula audio and disk
   DMA, CIA timers. The chip bus is arbitrated per colour clock, so a CPU
   chip-RAM access that loses arbitration genuinely waits
   ([](timing)).
3. When the CPU sits in `STOP`, the loop fast-forwards device time to the
   next event (timer underflow, raised interrupt) instead of spinning.
4. Render-relevant register writes (by Copper or CPU) are recorded as
   beam-position events. At presentation time the renderer replays them to
   paint the framebuffer; it never races the live chipset state
   ([](video)).
5. For the interactive window the loop sleeps to pace emulated time to
   wall-clock; for headless captures it does not. The emulated result is
   identical either way -- pacing only schedules host work.

## The Bus

`Bus` (`src/bus.rs`) owns all shared machine state: chip/slow/fast RAM and
ROM (`Memory`), both CIAs, the RTC, all custom-chip state (Agnus, Copper,
blitter, Paula, Denise), the floppy controller, the Zorro chain, and the
optional Gayle, Akiko, and CDTV subsystems. Everything routes through it;
there is exactly one owner for every register.

The CPU-visible memory map:

| Range | Contents |
|---|---|
| `$000000` - chip top | Chip RAM (512K-2M; ROM overlaid at `$0` after reset until CIA-A releases /OVL) |
| `$200000` - | Zorro II space (fast RAM autoconfig boards) |
| `$A00000` / `$BFxxxx` | CIA-A (odd bytes, /LDS) and CIA-B (even bytes, /UDS) |
| `$B80000` | Akiko (CD32 only) |
| `$C00000` | Slow ("ranger") RAM, up to 512K |
| `$DA0000` / `$DE1000` | Gayle IDE task file / Gayle ID and status (A600/A1200) |
| `$DC0000` | Battery RTC (MSM6242 view) |
| `$DFF000` | Custom-chip register window |
| `$E80000` | Zorro autoconfig window (then CDTV DMAC first on CDTV) |
| `$E00000` / `$F00000` | Extended ROM (CD32 / CDTV) |
| `$F80000` | Kickstart ROM (512 KiB) |
| `$40000000`+ | Zorro III space (32-bit CPUs) |

MMIO stubs cover every "gap" region DiagROM probes during fast-RAM
detection, so probing software sees bus-like behaviour everywhere.

## Determinism and the host boundary

The core's only inputs are the config, the loaded images, and the
timestamped input events (host keyboard/mouse/gamepad in windowed runs;
`--press-after`-style scripted events in headless runs). Audio is rendered
in emulated time and resampled at the host boundary; wall-clock affects
scheduling only. This is what makes `--screenshot-after` runs exactly
reproducible, lets the headless debugger replay a failure
deterministically, and makes [save states](savestate) exact: a restored
run is byte-identical to one that was never interrupted.

### Input scripting and recording (`inputrec.rs`)

Scripted input (`--press-after` and friends, or a `--script` file) fires
at the first frame boundary at-or-after each event's emulated timestamp.
The input recorder (window shortcut / `--record-input`) produces those
scripts from a live session by combining two capture styles: direct
hooks where event identity matters (the keyboard choke point
`handle_amiga_key_event`, floppy inserts) and a once-per-quantum diff of
the live `InputState` for port-1 mouse buttons, mouse motion (wrapped
quadrature-counter deltas become `mouse-after` directives), and the
port-2 joystick/CD32 pad. Two details keep record-then-replay
byte-identical: recorded timestamps are the emulated times the events
were *applied* (frame boundaries, not the wall-clock moment the host
delivered them), and times/holds are floored -- never rounded -- to
milliseconds, because rounding a boundary time up would push the
replayed event one frame late. The end-to-end gate is the same as for
save states: record a scripted run, replay the recording, `cmp` the
screenshots. User-facing usage is in
[](../guide/headless.md#input-recording-and-script-files).

## envcfg: environment variables are start-up settings

All `COPPERLINE_*` knobs are read through `src/envcfg.rs`, which snapshots
the entire environment once into a static map on first access. Hot paths
(per-instruction, per-cycle) consult these knobs; a live `std::env::var`
call there would take the process-wide environment lock millions of times a
second and starve the audio thread of the same lock.

Three consequences for contributors:

- Never call `std::env::var*` for a `COPPERLINE_*` knob -- use
  `envcfg::flag` / `envcfg::var`.
- Values cannot change at runtime; every knob is a start-up setting.
- On genuinely hot paths (per pixel, per colour clock, per device tick),
  even `envcfg::flag`/`envcfg::var` are too expensive: each call hashes the
  variable name to probe the snapshot map. Cache the value once through a
  `OnceLock` helper next to the call site -- see `dbg_cia_on()` and
  `no_disk_stall()` in `src/bus.rs` or `clamp_planes_setting()` in
  `src/video/bitplane.rs` for the pattern. (A per-pixel `envcfg::var` call
  in the playfield decoder once cost ~20% of total host CPU.)
