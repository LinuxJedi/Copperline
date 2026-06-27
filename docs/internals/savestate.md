# Save states (`savestate.rs`)

A save state snapshots the whole emulated machine to a single file and
restores it exactly. Because the core is deterministic
([](architecture.md#determinism-and-the-host-boundary)), a restored run
is byte-for-byte identical to one that was never interrupted -- the
regression gate for the feature literally `cmp`s screenshots taken on
either side of a save/restore cycle.

User-facing behaviour (shortcuts, menu items, the `--save-state-after` /
`--load-state` flags, and the operational caveats) is documented in the
[interactive UI guide](../guide/ui.md#save-states) and the
[headless-runs guide](../guide/headless.md#save-states-headless).
This chapter is the implementation and format reference.

## Design

The state is produced by `serde` derives on the live state structs
themselves -- there is no hand-maintained parallel "snapshot" schema.
`Bus` and everything it owns derive `Serialize`/`Deserialize`, as does
the vendored CPU core (`vendor/m68k`'s `CpuCore` is one flat struct of
plain registers and configuration; it has no lazily built decode tables,
so the whole struct round-trips). New fields are picked up by the
derives automatically; the cost of that convenience is the versioning
rule below.

What is captured:

| Component | Contents |
|---|---|
| `CpuCore` | registers, SR flags, prefetch queue, pending interrupt/stop state, MMU/CACR state, cycle-timing configuration, `cpu_type` and address mask |
| `MachineRuntimeState` | the `M68kMachine` fields outside the core: `last_cacr`, `sync_cck_on`, `cpu_clocks_per_cck`, `cpu_clock_carry` |
| `icache` / `dcache` | the 020/030 cache models (`Option<Box<CpuCache>>`, `None` when the model has no such cache or it is opted out). A snapshot with `None` for a cache the running CPU has -- an older state, or one taken before the caches defaulted on -- re-establishes that cache cold on load (enable bits re-derived from the restored CACR) instead of dropping it |
| `Bus` | chip/slow RAM, ROM and extended ROM, Zorro boards (including their RAM), both CIAs, RTC, Agnus/Copper/Denise/Paula/blitter state, floppy controller with in-memory disk images, Gayle IDE, A2091 SCSI, Akiko/CDTV with NVRAM, beam-event capture buffers, DMA pointers, interrupt latches, and the bus-arbitration counters |

Deliberately excluded, with the mechanism in parentheses:

- **Host sinks**: Paula's `Box<dyn SerialSink>` / `Box<dyn AudioSink>`
  (`#[serde(skip, default = ...)]` producing inert null sinks). On load,
  `Bus::adopt_host_resources` moves the live sinks from the old Bus onto
  the restored one, so audio output continues uninterrupted.
- **Diagnostic host state**: the `COPPERLINE_TRACE_BLITTER` file handle
  (skipped, moved across like the sinks), the debugger and its
  breakpoints/watchpoints (never serialized; they stay armed across a
  load), and the `dbg_*` instrumentation counters in `M68kMachine`.
- **Wall-clock anchors**: `DeviceClock::realtime_anchor` is an
  `Instant` and is skipped; it deserializes as `None` and
  `realtime_cck_due` lazily re-anchors to the host clock on first use.
  `Emulator::load_state` additionally re-baselines the frame pacer
  (`reanchor_realtime_clock`) so the run does not sprint to "catch up"
  the emulated-time jump. Live cpal output also drops any queued host
  frames from the abandoned timeline and rebuilds its prebuffer from the
  restored Paula stream; this is host presentation state, not serialized
  audio hardware.
- **Memo caches and transient diagnostics**: the bitplane slot-plan `Cell`
  cache and the pending debugger-window register hit (skipped; rebuilt or
  irrelevant).

The ROM bytes are embedded in the state, not loaded from a path: a state
is self-contained with respect to everything that was in memory, so
loading one always rebuilds *its own* machine -- restoring the Bus and
CPU restores the machine model along with them (the CPU bus adapter's
address-mask copy is re-synced from the restored core's `address_mask`).
A state loaded under a different config therefore does not corrupt
emulation; it silently *becomes* the machine the state was taken on.

To make that takeover visible and to keep host-side derived values in
step, the header carries a `MachineDescriptor` (`config.rs`): the
machine "shape" -- CPU model, chip/fast/slow RAM sizes, chipset
(OCS/ECS/AGA), video standard, and machine profile -- plus a fingerprint
of the boot and extended ROM (`RomId` = byte length + CRC-32 via
`flate2::Crc`). It is *not* a correctness gate (the Bus is authoritative);
it is the human-readable identity used to detect that a load swapped in
a different machine. On a mismatch `Emulator::load_state` logs the
field-by-field difference and **reconfigures the host to match the
state** rather than the now-stale running config:

- The frame pacer's cost-per-instruction is re-derived from the restored
  CPU clock (`cpu_clocks_per_cck`, which travels in `MachineRuntimeState`)
  via `cpu_cycles_per_instruction_for_clock`, so an accelerated or slower
  restored CPU is paced correctly. Presentation geometry already tracks
  the restored Bus (the renderer reads `bus().frame_geometry()` per
  frame), so PAL/NTSC and resolution follow automatically.
- The window surfaces the reconfiguration in its load OSD; headless runs
  report the loaded machine summary in the `save state loaded:` log line.

The ROM fingerprint is taken from the *in-memory* image (post
normalization -- a 256 KiB Kickstart 1.x mirrored up to 512 KiB), so the
running descriptor matches the bytes a save would embed. It is computed
from the `Bus`, not the `Config` (which holds only a path): main builds
the shape with `Config::descriptor()` and `Emulator::set_machine_descriptor`
fills the ROM fields from the live `Bus` via
`MachineDescriptor::set_rom_fingerprint`; `reload_rom` refreshes them when
the Kickstart is hot-swapped. Consequently a state taken on the same
machine shape but a *different* Kickstart is flagged on load (e.g. "ROM
512K:f6290043 -> 512K:fc24ae0d"). Storage image paths are deliberately
*not* fingerprinted, but missing storage is still caught on load: HDF/CD
images reopen by path and fail the load cleanly if absent (see below).

### File-backed images

Two subsystems hold open `File` handles inside otherwise serializable
state. Both get manual serde implementations through small shadow
structs, and both **reopen the file during deserialization**, which
turns a missing or moved image into a clean load-time error instead of a
later I/O panic:

- `HardDriveImage` (shared by Gayle IDE and A2091 SCSI) serializes as
  `HardDriveImageState { path, memory, total_sectors, rdb_overlay,
  overlay_write_warned, scsi_bus }`. A file-backed image stores
  `memory: None` and reopens `path` read/write on load; an in-memory
  directory-built volume stores the whole image in `memory`, so its
  session-only writes survive the round trip. The synthesized-RDB
  overlay for bare hardfiles is embedded either way. Consequence: HDF
  *file contents* are not part of the state -- guest writes made after
  the snapshot are still visible after restoring.
- `CdImage` serializes as `CdImageState { paths, tracks, extents,
  total_sectors }` and reopens every image file read-only on load
  (`CdImage` now keeps the per-file `paths` alongside its `files`
  for exactly this purpose).

Floppy images need no special handling: `FloppyImage` keeps its data
in memory (`StandardAdf(Vec<u8>)` or per-track structures), so inserted
disks travel inside the state, unsaved track writes included.

## File format

```
offset  size  contents
0       8     magic, ASCII "CLSSTATE"
8       4     format version, u32 little-endian (STATE_VERSION)
12      ...   MachineDescriptor, bincode (uncompressed)
...     ...   zlib stream (RFC 1950) containing the payload
```

The `MachineDescriptor` sits uncompressed ahead of the zlib stream so a
load can read it (and detect a machine mismatch) without inflating the
whole machine; bincode consumes exactly its encoded bytes, leaving the
reader positioned at the start of the zlib stream. `savestate::load`
returns the descriptor to `Emulator::load_state` for the comparison.

The payload inside the zlib stream is five bincode values written
back-to-back by `M68kMachine::write_state`, in this fixed order:

1. `CpuCore`
2. `MachineRuntimeState`
3. `icache: Option<Box<CpuCache>>`
4. `dcache: Option<Box<CpuCache>>`
5. `Bus`

`M68kMachine::apply_state` reads them back in the same order and only
swaps the machine onto the parsed state after every component has
deserialized, so a truncated or corrupt file leaves the live machine
untouched (`savestate::tests::truncated_payload_leaves_the_machine_untouched`).

Encoding details, for anyone reading a state file from outside:

- bincode 1.x legacy defaults: little-endian, **fixed-width** integers
  (`u16` is 2 bytes, `u32` 4, `usize` 8), `bool` as one byte,
  `Option<T>` as a one-byte tag (0/1) followed by the value, enum
  variants as a `u32` index, and `Vec`/`String`/`PathBuf` as a `u64`
  length prefix followed by the elements/UTF-8 bytes.
- Arrays larger than 32 elements go through `serde-big-array` (the
  AGA palette's two `[u16; 256]` nibble planes, autoconfig ROM images,
  CPU-cache line arrays); on the wire they are simply the elements in
  order, like any other array.
- The payload is **not self-describing**: the schema is the Rust
  structs at the `STATE_VERSION` that wrote the file. There are no field
  names or tags in the stream.
- Compression is `flate2` at `Compression::fast()`; any standard zlib
  inflater reads it regardless of level. A Kickstart 2.05 machine
  (512K chip + 512K ROM) compresses to roughly 400 KB.

## Versioning

`STATE_VERSION` (in `savestate.rs`) is compared exactly on load; a
mismatch fails with a message naming both versions. Because the payload
is positional bincode of the live structs, **any** shape change to any
serialized struct -- a field added, removed, reordered, or retyped
anywhere under `Bus`, the chipset modules, `CpuCore`, floppy or
expansion state, *or the header `MachineDescriptor`* -- silently changes
the wire layout. The rule is
therefore: bump `STATE_VERSION` whenever such a change lands, so stale
files are refused with a clear version message instead of failing with a
confusing decode error (or worse, decoding into nonsense). There is no
migration machinery; old states are simply invalidated.

## Snapshot point and atomicity

The app-level contract is that states are taken at presentation-quantum
boundaries: the window event loop and the headless timers both act only
after `step_frame` returns, and `--save-state-after` fires at the first
quantum past its deadline. A quantum can land inside an emulated field, so
the serialized surface must round-trip any inter-instruction point (the
unit test saves mid-frame after arbitrary `step_slice` counts). When a load
resumes anywhere other than the start of a field, the hardware state
continues exactly from that beam position, but the renderer marks that
partly reconstructed field non-presentable and waits for the next complete
field before updating screenshots, frame dumps, or the window.

`savestate::save` takes `&M68kMachine` and does not mutate emulated
state. `savestate::load` parses fully before applying, then moves host
resources across, resets any queued live-audio presentation frames from the
old timeline, and clears transient video capture buffers. The restored guest
RAM, custom registers, and beam event journal stay intact, while Agnus
rebuilds sprite control/data latches from the restored pointer context under
the current descriptor rules. Register-armed sprite streams whose transient
descriptor latch was not serialized are reconstructed from Denise's retained
SPRxPOS/SPRxCTL/data-armed state and the next after-slot SPRxPT low-word
write in the rendered field, so the first complete field after load follows
the same data-stream rule as a live run. On success the window forces power
on, clears any CPU halt latch, and invalidates `last_rendered_emulated_frame`
so the next presentation re-renders from the restored Bus.

## Verification

The determinism gate lives at three levels:

- `cpu::tests::save_state_round_trip_replays_identically`: runs a
  chip-RAM loop that also writes COLOR00 (so CPU, RAM, and beam-event
  capture all advance), saves at T1, runs 20k instructions to T2 saving
  the state again, rewinds to T1, replays the same step pattern, and
  asserts the trace matches **and the re-serialized T2 state file is
  byte-identical** to the original timeline's.
- `savestate::tests` cover magic/version rejection, the
  truncated-payload atomicity guarantee, the header descriptor round
  trip (`round_trips_the_machine_descriptor`), and that a CD controller
  travels in the state so the bar's CD controls appear on load
  (`cd_controller_travels_in_the_state`);
  `config::tests::rom_fingerprint_distinguishes_same_shape_kickstarts`
  covers flagging a swapped same-shape Kickstart;
  `emulator::tests::pacing_cost_scales_with_cpu_clock` covers the
  host-pacing re-derivation a mismatched load performs; `harddrive::tests`
  and `cdrom::tests` cover the reopen-by-path round trips and the
  missing-file error paths.
- End-to-end: save mid-run plus `--screenshot-after T`, then
  `--load-state` plus `--screenshot-after T` in a fresh process, and
  `cmp` the PNGs. Verified byte-identical on Kickstart 2.05, State of
  the Art mid-demo (floppy and blitter state in flight), and A1200
  Workbench (AGA, Gayle).

## Reverse debugging (`timetravel.rs`)

Reverse debugging is built directly on this determinism. Where `rr`
records every nondeterministic syscall and signal to make replay
reproducible, Copperline already has that for free, so going backwards is
just *snapshot + replay*: keep a ring of recent machine states, and to
reach an earlier point restore the nearest one at or before it and replay
forward. The user-facing surfaces (headless `COPPERLINE_DBG_RWATCH`, the
window's **&lt; Step** / **&lt; Run**) are documented in
[](../debugger/reverse.md); this section is the model.

### Snapshot ring

`SnapshotRing` (in `timetravel.rs`) holds `Snapshot { pos, frame, blob }`
entries, captured by `Emulator::tt_capture_if_due` at frame boundaries --
the same quiescent point save states require. The `blob` is produced by
`M68kMachine::write_state` into a `Vec`, **bypassing the zlib + magic +
version framing** of a file save state: snapshots live and die inside one
process running one binary, so format compatibility is a non-issue and
skipping it keeps capture cheap. Captures are taken every
`COPPERLINE_DBG_RR_INTERVAL` frames and the oldest are evicted once the
total blob size passes `COPPERLINE_DBG_RR_BUDGET_MB`; the ring never drops
below one anchor.

### Position coordinate

Reverse ops navigate by `Emulator::retired_instructions`, a monotonic
count bumped per retired instruction in `execute_cpu_slice`. It lives
**outside** the serialized state -- paired with each snapshot in the ring,
not inside the blob -- so it costs nothing to capture and, importantly,
**does not change the save-state shape**: reverse debugging needs no
`STATE_VERSION` bump. A reverse step to position *P* restores the nearest
snapshot with `pos <= P` and single-steps to *P* through `run_one_step`,
the exact per-instruction body the forward `step_real` loop uses (factored
out so replay reproduces the forward run instruction-for-instruction,
including the `STOP`-state idle fast-forward).

### Input replay (`inputsched.rs`)

Replay is only byte-identical if input is reproduced at the position it was
applied. The live forward run keeps applying input exactly as before; when
reverse mode is armed it also *records* each action into a position-keyed
`ReplayInputLog` (`Emulator::tt_note_input`, called from the central
keyboard / mouse-button / mouse-motion / joystick helpers, through which
both scripted and window input funnel). During replay the engine re-applies
logged actions as it reaches their positions. A floppy media change is
logged as a marker that warns on replay rather than silently diverging (the
inserted image is host-file state, not in the log).

### Determinism boundaries

The same host boundary as save states applies, plus the requirement that
*time-dependent* inputs be pinned, since replay re-executes them:

- The guest RTC (`rtc.rs`) reads host wall-clock time unless
  `COPPERLINE_RTC_FIXED_SECS` is set; reverse mode warns when it is unset.
- Directory-backed (host-folder) filesystems stamp guest-visible host
  datestamps with no fixed-time override -- avoid for reverse replay.
- HDF/CD images reopen by path and are externally mutable, so a guest disk
  write after a snapshot is not rolled back by restoring it; floppy
  contents are in-state and safe.

### Verification

- `cpu::tests::reverse_step_reconstructs_earlier_state_and_finds_last_writer`
  reverse-steps then replays forward to the original position and asserts an
  exact match, and pins the unique writer of a counter word.
- `cpu::tests::reverse_replay_reproduces_logged_input` proves a logged mouse
  motion is re-applied when replayed through from an earlier snapshot.
- `cpu::tests::reverse_watchpoint_does_not_disturb_the_forward_run` runs the
  state-mutating query bracketed by snapshot/restore and asserts a run with
  the watch armed matches one without it.
- `timetravel::tests` cover the ring's interval/eviction/lookup policy;
  `inputsched::tests` cover the replay-log cursor and pruning;
  `window::tests::opening_the_debugger_arms_reverse_and_step_reconstructs`
  drives the window controls.
