# Architecture overview

This chapter describes the source layout, frame loop, and host boundary.
Related chapters cover the
[timing model](timing.md), the [chipset modules](chipset.md), the
[video pipeline](video.md), the [CPU integration](cpu.md), the
[peripherals](peripherals.md), and the [save-state format](savestate.md).

## Source layout

```
src/
  main.rs           # native binary: config load, boot, execution-mode selection
  cli.rs            # binary-local command-line parser
  bin/bench.rs      # copperline-bench headless benchmark (native + wasm32-wasip1)
  lib.rs            # library entry point shared by native and browser frontends
  config/           # TOML config + validation + machine profiles
                    #   (mod.rs, raw.rs, validate.rs, resolve.rs, about.rs, tests.rs)
  envcfg.rs         # cached COPPERLINE_* environment-variable snapshot
  emulator.rs       # frame loop driving CPU, chipset, and host I/O
  debugger.rs       # env-driven headless debugger
  waveform.rs       # trigger-based VCD chipset-signal capture (GTKWave)
  disasm.rs         # 68000 + Copper-list disassemblers
  gdbstub/          # GDB remote-protocol stub (transport-free core, headless and windowed drivers)
  control/          # control protocol (JSON-RPC over TCP), its MCP server and DAP adapter (control/dap/)
  debuginfo/        # guest program debug information: hunk symbols, LINE hunks, DWARF, unwinding
  amigaos.rs        # read-only exec.library structure walking for the debugger
  cpu.rs            # m68k core wrapper and CPU-visible bus adapter
  cache.rs          # 68020/030/040 on-chip instruction/data cache model
  bus.rs            # shared RAM, ROM, chipset, CIA, RTC, and I/O state
  bus/              # size-split impl Bus continuations + the bus test suite
    custom_regs.rs  #   custom-chip register read/write dispatch
    dma_slots.rs    #   chip-bus slot arbitration + DMA scheduling
    ddf_line.rs     #   per-line DDF sequencer walk for slot planning
    collisions.rs   #   live (beam-timed) collision accumulation
    frame_capture.rs#   per-frame sprite/bitplane DMA + render capture
    wave.rs         #   waveform-capture signal sampling hooks
  memory.rs         # chip/slow RAM, ROM, extended ROM containers
  romsearch.rs      # locate the bundled AROS default boot ROM
  zorro.rs          # Zorro II/III autoconfig chain and boards
  zorro_device.rs   # the functional-Zorro-board boundary (ZorroDevice trait)
  wasmboard.rs      # WASM plugin boards under wasmtime (wasm-boards feature)
  wasm_manifest.rs  # plugin manifest types shared with wasmtime-less builds
  floppy/           # timed disk DMA controller + disk image decoding
                    #   (mod.rs controller/drive core, formats.rs images, tests.rs)
  dms.rs            # DMS archive decompression
  gzip.rs           # gzip member loop behind ADZ/HDZ (multi-member, padding-tolerant)
  drive_sounds.rs   # synthesized floppy-drive sound effects
  gary.rs           # Gary motherboard address decode (big-box machines)
  ramsey.rs         # Ramsey memory controller registers (A3000/A4000)
  gayle.rs          # A600/A1200 Gayle gate array + IDE
  ata.rs            # shared ATA task-file/drive model behind Gayle and A4000 IDE
  ide_a4000.rs      # A4000 motherboard IDE at $DD2020
  a2091.rs          # A2091 SCSI controller board (DMAC + boot ROM)
  a4091.rs          # A4091 Zorro III SCSI-2 controller (NCR 53C710)
  scsi.rs           # WD33C93A SBIC + SCSI-2 disk targets
  copperhf.rs       # virtual hardfile controller with ordered worker-thread I/O
  sdmac.rs          # A3000 Super DMAC fronting the WD33C93
  harddrive.rs      # shared hard-drive image backend (IDE + SCSI)
  dirfs.rs          # host directory -> in-memory FFS/OFS partition image
  filesys.rs        # host directories mounted live as AmigaDOS volumes
  a2065.rs          # A2065 Zorro II Ethernet board (Am7990 LANCE)
  net/              # loopback, userspace NAT, and host-adapter bridge backends
  cdrom.rs          # CD images: cue sheets (BINARY/WAVE/MP3), ISO, NRG, CHD
  cdtv.rs           # CDTV DMAC + Matshita drive model
  akiko.rs          # CD32 Akiko (C2P, NVRAM, Chinon drive)
  cd32_fmv.rs       # CD32 FMV cartridge (CL450 video + L64111 MPEG audio)
  rtc.rs            # MSM6242 and RP5C01 battery RTCs
  serial.rs         # Paula serial sinks (stdout, TCP, pty)
  midi/             # host MIDI serial bridge (CoreMIDI / ALSA / WinMM)
  audio/            # AudioSink outputs, source/stem mux, and resampler
  priority.rs       # opt-in realtime-like thread scheduling (pacer + audio)
  gamepad.rs        # gilrs input: database layout + calibration override
  screenshot.rs     # PNG export helpers
  recorder.rs       # video+audio capture (ZMBV/PCM AVI writer)
  inputrec.rs       # live-input recording to the scripted-input format
  inputsched.rs     # deterministic input replay for reverse debugging
  savestate.rs      # whole-machine snapshot/restore (versioned file format)
  timetravel.rs     # reverse-debugging snapshot ring + replay
  timestamp.rs      # compact wall-clock stamps for generated filenames
  timebase.rs       # host clock imports shared with the browser build
  chipset/
    agnus.rs        # beam counters, DMACON, display fetch, arbitration data
    copper.rs       # Copper decode + cycle-stepped execution
    ddf_sequencer.rs#   Agnus DDF comparator/flop model (bitplane fetch gating)
    blitter.rs      # scheduled per-DMA-slot blitter engine
    paula.rs        # interrupts, audio DMA, serial, disk regs
    denise.rs       # palette + bitplane/sprite control registers
    cia.rs          # 8520 CIA model (CIA-A and CIA-B)
    keyboard.rs     # bit-timed 6500/1 keyboard MCU into CIA-A
  video/
    beam.rs         # beam-position event index for the renderer
    bitplane.rs     # event replay + planar->RGBA renderer
    bitplane/       # size-split renderer submodules + its test suite
                    #   (sprite.rs, fetch.rs, output.rs, diag.rs, tests.rs)
    deinterlace.rs  # motion-adaptive deinterlacer
    present_common.rs # frontend-independent post-processing + TV apertures
    window.rs       # winit ApplicationHandler + render worker + main/tool pixels surfaces + status bar
    window/         # size-split App impl blocks + window submodules + its test suite
                    #   (app_input.rs, app_menus.rs, app_debugger.rs, app_launcher.rs,
                    #    app_session.rs, app_display.rs, app_panels.rs, app_media.rs,
                    #    statusbar.rs, present.rs, host_input.rs, console.rs, tests.rs)
    ui.rs           # pop-up menu, overlay panels, and debugger/analyzer panel drawing
      configuration/ # configuration rows, library rendering and dialogs
    launcher.rs     # machine-configuration (launcher) editable model
    launcher/       # field metadata, value edits and config conversion
    font.rs         # 8x8 overlay font
crates/copperline-web/   # standalone wasm-bindgen browser frontend (WebEmu + page glue)
crates/copperline-libretro/ # standalone libretro C ABI frontend
crates/cputest-runner/   # WinUAE cputest instruction-suite runner for the m68k core
tests/              # self-contained integration tests and ignored local-asset tests
timing-test/        # bootable cross-emulator timing-measurement disk
```

## The big picture

Copperline's emulated machine runs synchronously on the main thread inside
winit's event loop. There is no emulation thread and no locking between CPU
and chipset: each turn of the loop (`Emulator::step_real`,
`src/emulator.rs`) advances the deterministic core a frame's worth of
emulated time, cycle-stepping the CPU and the chipset together. By default,
the completed-frame renderer runs one frame behind on a worker thread; set
`COPPERLINE_THREADED_RENDER=0` to use the synchronous renderer for
comparison.

The winit window is only the default frontend. With the default `frontend`
cargo feature disabled, the crate builds as the portable headless core plus
the frontend-independent presentation helpers (`video/present_common.rs`),
with no desktop dependencies; that is the surface the browser (WebAssembly)
frontend in `crates/copperline-web` wraps
([](../guide/browser.md)), and `cargo check --no-default-features` is the
CI-enforced portability invariant. Native default builds also enable the
`profile-stats` feature for detailed MMIO-poll and render-phase diagnostics.
The browser dependency deliberately leaves it disabled: browser status still
measures coarse core and render cost at the wrapper boundary, but the
instruction and renderer hot paths do not maintain native-only counters or
sample host clocks.

The [libretro frontend](../guide/libretro.md) builds against that same portable
surface. Its caller supplies pacing and synchronous audio/video/input callbacks.
`Emulator::step_video_frame` advances to the next hardware field using the
precise stepping path shared with netplay, whereas desktop `step_frame` may
end a CPU-budget quantum within a field. `Agnus::nominal_frame_cck` supplies
the mean field length, including alternating interlace fields and NTSC lines,
for frontend refresh reporting. The adapter uses the shared renderer and TV
presentation helpers, keeps floppy writes in memory until eject/unload, and
wraps ordinary machine state bytes with playlist and pending-input state.
`savestate::read_descriptor` lets it check the stored machine configuration
before restoring the payload, without changing the save-state format.

The flow of a frame:

1. The frame loop hands the CPU an instruction budget. The CPU executes
   one instruction at a time through the published `m68k` core; every memory
   access the instruction makes is routed through the bus adapter and
   *billed in colour clocks* (CCK, 3.546895 MHz -- the chip bus clock).
   The precise CPU loop selects its diagnostic-capable or branch-free normal
   form once per slice. Arming a debugger, trace, watchpoint, waveform PC
   trigger or diagnostic recorder selects the former; normal and browser
   runs therefore do not retest every inactive hook after every instruction.
   An instruction-granular control stop retains the unfinished budget and its
   `STOP` fast-forward state in `Emulator`; resume completes that same quantum
   before starting another, so a host-side observation cannot move an
   emulated interrupt race by repartitioning the execution loop.
2. Advancing the clock for a CPU access also advances everything else:
   Agnus beam counters, Copper fetches, blitter slots, Paula audio and disk
   DMA, CIA timers. The chip bus is arbitrated per colour clock, so a CPU
   chip-RAM access that loses arbitration genuinely waits
   ([](timing)).
3. When the CPU sits in `STOP`, the loop fast-forwards device time to the
   next event (timer underflow, raised interrupt) instead of spinning.
4. Render-relevant register writes (by Copper or CPU) are recorded as
   beam-position events. At the frame boundary, the bus turns the completed
   frame's events, chip-RAM snapshot, display geometry, and Agnus blanking
   latches into an owned `RenderInput` envelope. Large immutable RAM and
   bitplane-row snapshots are reference-counted across the handoff; the
   renderer replays that frozen data, never the live chipset state
   ([](video.md)).
5. In the default path `window.rs` sends `RenderInput` to the
   `copperline-render` worker while the main thread advances the next frame.
   The worker paints into a CPU framebuffer, owns the deinterlacer history,
   and returns a presentation buffer tagged with the emulated frame. GPU
   upload and winit/window operations stay on the main thread.
6. For screenshots, frame dumps, video recording, debugger stepping, and
   run-to-PC commands, the window code waits for the worker result for the
   exact emulated frame being captured or inspected. For normal interactive
   display, one frame of presentation latency is allowed.
7. For the interactive window the loop sleeps to pace emulated time to
   wall-clock; for headless captures it does not. The emulated result is
   identical either way -- pacing only schedules host work.

The main presentation path uses the **main thread** (event loop, core,
and pacer), the **`copperline-render` worker**, and the **cpal audio
callback**. Rendering passes owned buffers and immutable shared snapshots;
audio passes samples through a lock-free ring buffer. Enabled devices and
services can create additional workers, including copperhf file I/O and
debugger socket handling. The pacer and the audio callback are
latency-critical and can optionally be given above-normal scheduling priority
(`[emulation] realtime_priority`, `src/priority.rs`); see
[](timing) for what that does per platform.

## The Bus

`Bus` (`src/bus.rs`) owns all shared machine state: chip/slow/fast RAM and
ROM (`Memory`), both CIAs, the RTC, all custom-chip state (Agnus, Copper,
blitter, Paula, Denise), the floppy controller, the Zorro chain, and the
optional Gayle, Akiko, and CDTV subsystems. Everything routes through it;
there is exactly one owner for every register.

The CPU-visible memory map:

| Range | Contents |
|---|---|
| `$000000` - chip top | Chip RAM (256K-2M; ROM overlaid at `$0` after reset until CIA-A releases /OVL) |
| `$200000` - | Zorro II space (fast RAM autoconfig boards) |
| `$A00000` / `$BFxxxx` | CIA-A (odd bytes, /LDS) and CIA-B (even bytes, /UDS) |
| `$B80000` | Akiko (CD32 only) |
| `$C00000` | Slow ("ranger") RAM, up to 512K |
| `$DA0000` / `$DE1000` | Gayle IDE task file / Gayle ID and status (A600/A1200) |
| `$DC0000` | Battery RTC (MSM6242 or RP5C01, according to configuration) |
| `$DFF000` | Custom-chip register window |
| `$E80000` | Zorro autoconfig window (then CDTV DMAC first on CDTV) |
| `$E00000` / `$F00000` | Extended ROM (CD32 / CDTV) |
| `$F80000` | Kickstart ROM (512 KiB) |
| `$40000000`+ | Zorro III space (32-bit CPUs) |

MMIO stubs cover every "gap" region DiagROM probes during fast-RAM
detection, so probing software sees bus-like behaviour everywhere.

On the small-box machines (A1000/A500/A2000/CDTV), Gary's coarse decode
mirrors the custom-chip registers throughout the chip-register space where
nothing else answers: the ranger space above the fitted slow RAM in
`$C00000`-`$D7FFFF`, the reserved `$D80000`-`$DBFFFF` pages, and
`$DE0000`-`$DFEFFF` (`CpuBus::custom_reg_page_offset`). Kickstart's
slow-RAM sizing depends on this: exec probes each 256K step by writing
INTENA at `$xxF09A` and reading INTENAR at `$xxF01C`, extending RAM until
the mirror answers -- on a floating bus KS 1.2 sizes RAM into unmapped
space and dies on a yellow screen. Machines with a Gayle or Fat
Gary/Ramsey decode the space properly and have no mirror.

A CPU read of an address no region claims floats to the last value the
chip data bus carried (`Bus.data_bus`, fed by the live display and audio
DMA fetches), as on real Agnus-arbitrated hardware -- not a fixed all-ones
pattern; on a blank screen that value is 0. The constant matters: software
that chases a pointer off into unmapped space (e.g. a filesystem walking a
corrupted buffer-cache chain) can loop forever on a fixed value, where the
ever-changing floating value lets the chase wander to a zero terminator as
on silicon. Device windows that decode their own floating bus keep their
own values -- e.g. the A2091 board's unpopulated XT-interface bytes read
`$FF` from its own model, not the chip data bus.

Write-only and unmapped custom-register offsets float the same way: the
chips decode the address but drive no data, so `move.w $DFF106,d0` returns
the bus residue rather than the register's internal latch (which the
debugger still inspects separately). Each driven word cycle recharges the
bus, so a longword read pairing a readable register with a write-only one
floats the second word to the first -- `move.l $DFF01E,d0` reads INTREQR
in both halves. Reading a write-only register back
and OR-ing the result into a fresh write therefore picks up garbage bits
exactly as on real hardware -- a floating BPLCON3 LOCT bit, for example,
misroutes AGA palette writes into the low nibbles and darkens the whole
palette. The one deliberate exception is DENISEID (`$07C`) on OCS Denise,
held at `$FFFF` so ECS-detection code (low byte `$FC`) cannot false-match
a residue; see the TODO in `read_custom_word`.

## Direct executable boot support

`runprog.rs` stages `RunBoot:` and mounts the executable directory as
`RunProg:`. The generated Startup-Sequence runs through the guest's normal
CLI and LoadSeg path, which also supplies the debugger's hunk relocation
and entry stop. It includes the relocatable 68000 commands built from
`guest/run-tools/`: `FailAt`, `CD`, `Stack`, `Echo`, and `Execute`. Their checked-in
executables are embedded in Copperline and written into the boot volume's
`C` directory, so installations need no external command archive or compiler.

The commands use Exec/DOS 1.x calls and public Process/CLI fields. They change
the CLI failure threshold and default command stack, acquire the working
directory lock, and write the completion marker through ordinary DOS I/O.
CLI, DAP, and staging share stack-size validation matching the guest helper,
so an unsupported size cannot be silently ignored by the startup script.
No ROM identification, host-side state patch, or emulator trap is involved;
newer shells resolve their internal commands normally. The optional detached
path uses the ROM's `Run`/`EndCLI` commands and requires Kickstart 2.0+ or
AROS. The bundled `Execute` hands the generated script to the child CLI;
it rejects nested scripts and does not perform parameter substitution.
See [direct launching](../guide/run.md).

## Determinism and the host boundary

With fixed configuration, media, clock seeds, and timestamped inputs,
the core advances deterministically. Audio is generated in emulated time;
window pacing and render-worker scheduling do not change the guest timeline.
This supports repeatable headless captures and snapshot replay.

Host services can supply additional inputs or retain effects outside a
snapshot:

- A fitted RTC reads the host's local time unless seeded with
  `[machine] rtc_time` / `--rtc-time`. A seed advances in emulated time;
  `--rtc-frozen` holds it constant. `COPPERLINE_RTC_FIXED_SECS` is the
  older fixed-UTC override.
- Live host-directory mounts expose file contents and timestamps. File-backed
  hard disks and physical disks retain writes made after a snapshot.
- Network, serial, MIDI, and sampler input can depend on external devices
  or services. Physical floppy drives also require wall-clock pacing.

Use fixed image files and scripted inputs for deterministic regression
tests. See [save-state boundaries](savestate.md#determinism-boundaries) for
what a restore can reproduce. Some audio decoders also have
[platform-dependent floating-point rounding](mhi.md#copperline-implementation-notes).

### Input scripting and recording (`inputrec.rs`)

Scripted input (`--press-after` and friends, or a `--script` file) fires
at the first frame boundary at-or-after each event's emulated timestamp.
The input recorder (window shortcut / `--record-input`) produces those
scripts from a live session by combining two capture styles: direct
hooks where event identity matters (the keyboard choke point
`handle_amiga_key_event`, floppy inserts) and a once-per-quantum diff of
the live `InputState` for each port's configured device: mouse buttons and
motion, joystick/CD32 controls, or analogue positions. Wrapped quadrature
counter deltas become `mouse-after` directives. Port-device changes themselves
are not recorded, so replay must use the same port configuration. Two details keep record-then-replay
byte-identical: recorded timestamps are the emulated times the events
were *applied* (frame boundaries, not the wall-clock moment the host
delivered them), and times/holds are floored -- never rounded -- to
milliseconds, because rounding a boundary time up would push the
replayed event one frame late. The end-to-end gate is the same as for
save states: record a scripted run, replay the recording, `cmp` the
screenshots. User-facing usage is in
[Input recording and scripts](../guide/headless.md#input-recording-and-script-files).

## Guest debug information

The debug-information loader maps ELF allocatable sections onto hunks in
elf2hunk's section order. For relocatable ELF (`ET_REL`), whose sections can
all have address zero, it assigns distinct internal 32-bit addresses and
materializes DWARF and call-frame sections with `R_68K_32` / `R_68K_PC32`
relocations applied. Section symbols and ordinary symbols retain their
section identity; references to debug sections remain section offsets.
RELA uses its explicit signed addend, while REL uses the stored big-endian
word. These internal addresses map back to hunk offsets and then to the
runtime bases reported by LoadSeg; guest memory is untouched. Linked ELF
keeps its linker addresses and already-relocated section bytes, including
when `--emit-relocs` retains relocation records. Unsupported relocations
or invalid relocation targets/offsets produce a section-specific diagnostic.
See the [DAP reference](../debugger/dap.md#debug-information) for supported
toolchains and variable-location expressions.

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
