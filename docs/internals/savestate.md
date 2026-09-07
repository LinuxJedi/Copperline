# Save states (`savestate.rs`)

A save state stores the emulated machine in a single file. With the same
external media and repeatable inputs, restoring it reproduces the guest
timeline. Host files, physical devices, and live services have separate
[replay limits](#determinism-boundaries); they are not rolled back with RAM.

User-facing behaviour (shortcuts, menu items, the `--save-state-after` /
`--load-state` flags, and the operational caveats) is documented in the
[interactive UI guide](../guide/ui.md#save-states) and the
[headless-runs guide](../guide/headless.md#save-states-headless).
This chapter is the implementation and format reference.

## Design

Most state uses `serde` derives on the live structs, including `Bus` and
the published `m68k` core. Types with file handles, decoder internals, or
feature-dependent board variants use custom serialization. `CpuCore`
serializes architectural state, prefetch state, MMU state, and timing
configuration; runtime-only decoded-op, FastMem, and trace-JIT caches are
skipped and rebuilt after deserialization. New fields are picked up by the
derives automatically, and because a state file names its fields, most
struct changes need no version at all (see [Versioning](#versioning)).
In a file the `Bus` is split into one chunk per subsystem; in memory it
is one struct, and the split is a serialization-time view.

What is captured:

| Component | Contents |
|---|---|
| `CpuCore` | registers, SR flags, prefetch queue, pending interrupt/stop state, MMU/CACR state, cycle-timing configuration, `cpu_type` and address mask |
| `MachineRuntimeState` | the `M68kMachine` fields outside the core: `last_cacr`, `sync_cck_on`, `cpu_clocks_per_cck`, `cpu_clock_carry` |
| `icache` / `dcache` | the CPU cache models (`Option<Box<CpuCache>>`, `None` when absent or opted out). If a compatible state lacks a cache that the restored CPU configuration requires, load creates it cold with enable bits derived from CACR |
| `Bus` | chip/slow RAM, ROM and extended ROM, Zorro boards (including their RAM), both CIAs, RTC, Agnus/Copper/Denise/Paula/blitter state, floppy controller with in-memory disk images, Gayle IDE, A2091 SCSI, Akiko/CDTV with NVRAM, beam-event capture buffers, DMA pointers, interrupt latches, and the bus-arbitration counters |

Deliberately excluded, with the mechanism in parentheses:

- **Host sinks and taps**: Paula's `Box<dyn SerialSink>`, `AudioMux`,
  and the bounded CCP serial-observation tap
  (`#[serde(skip)]`; the sink defaults produce inert null devices). On load,
  `Bus::adopt_host_resources` moves the live sinks and tap from the old Bus
  onto the restored one, so output and an active subscription continue
  uninterrupted.
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
  cache, the pending debugger-window register hit, and the low-memory
  blit crash-context alarm (`diag_lowmem_blit`, consumed only by
  `COPPERLINE_DIAG_CRASH`) are skipped (rebuilt or transient). Excluding
  host diagnostics from the state payload ensures that resumed runs and
  uninterrupted runs produce byte-identical save states
  (verified by `tests/savestate_roundtrip.rs`).

The ROM bytes are embedded in the state, not loaded from a path: a state
is self-contained with respect to everything that was in memory, so
loading one always rebuilds *its own* machine -- restoring the Bus and
CPU restores the machine model along with them (the CPU bus adapter's
address-mask copy is re-synced from the restored core's `address_mask`).
A state loaded under a different config restores the saved machine model.

To make that takeover visible and to keep host-side derived values in
step, the header carries a `MachineDescriptor` (`config/mod.rs`): the
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

Hard-drive and CD backends store enough metadata to reopen their backing
files during deserialization. Missing or moved images cause a load-time error:

- `HardDriveImage` (shared by IDE, SCSI, and copperhf) serializes as
  `HardDriveImageState { path, memory, total_sectors, rdb_overlay,
  overlay_write_warned, scsi_bus, host_device }`. A file-backed image stores
  `memory: None` and reopens `path` read/write on load; an in-memory
  directory-built volume stores the whole image in `memory`, so its
  session-only writes survive the round trip. The synthesized-RDB
  overlay for bare hardfiles is embedded either way. Consequence: HDF
  *file contents* are not part of the state -- guest writes made after
  the snapshot are still visible after restoring.
- `CdImageState::Bin { sources, tracks, extents, total_sectors }` stores
  the source descriptions needed to reopen BIN/WAVE/MP3, ISO, and NRG data.
  `CdImageState::Chd { path }` stores the CHD path and rebuilds the CHD
  backend on load. Both reopen media read-only.
- A physical disk stores its identifier, fingerprint, and write-access
  setting in `host_device`. Deserialization creates a pending device;
  the host-disk reopen path then checks its identity and access rules.
  It is never reopened as an ordinary file. Browser builds reject these states.

Floppy images need no special handling: `FloppyImage` keeps its data
in memory (`StandardAdf(Vec<u8>)` or per-track structures), so inserted
disks travel inside the state, unsaved track writes included.

## File format

```
offset  size  contents
0       8     magic, ASCII "CLSSTATE"
8       4     container version, u32 little-endian (STATE_VERSION, 81)
12      ...   DESC chunk: the MachineDescriptor, uncompressed
...     ...   zlib stream (RFC 1950) of chunks, ending in an END chunk
```

Every chunk, in the clear or inside the zlib stream, is framed the same
way:

```
offset  size  contents
0       4     tag, four ASCII bytes ("PAUL", "BUS ", "END ")
4       4     chunk version, u32 little-endian
8       8     payload length, u64 little-endian, or all ones
16      ...   payload
```

A payload length of all ones (`chunk::STREAMED`) means the writer did not
know the length up front and the payload follows as a run of blocks, each
a `u32` little-endian length followed by that many bytes, ended by a
zero-length block. Blocks are at most 1 MiB. The `Bus` chunks are written
this way; the descriptor and the value chunks carry their length. A reader
accepts either form for any chunk.

The `DESC` chunk sits uncompressed ahead of the zlib stream so a load can
read it (and detect a machine mismatch) without inflating the whole
machine; `savestate::load` returns the descriptor to `Emulator::load_state`
for the comparison. `savestate::inspect` reads the header, the descriptor,
and the chunk directory of a file without restoring anything.

The zlib stream holds the chunks `M68kMachine::write_chunks` produces, in
this order (the `Bus` chunks follow the order of their fields in `Bus`,
with the catch-all last), then the `END ` marker: version 0, empty, and
followed by nothing but the end of the zlib stream. A stream cut short
before the marker is refused; data after it is refused; reading up to the
end of the stream is what makes the zlib decoder verify its trailer
checksum. A file cut off after the marker still loads, since every chunk
before it arrived complete.

| Tag | Payload | Contents |
|---|---|---|
| `CPU ` | value | `CpuCore` |
| `MACH` | value | `MachineRuntimeState` |
| `ICAC`, `DCAC` | value | `Option<Box<CpuCache>>`, `nil` when absent |
| `MEM ` | bus fields | `mem` (chip/slow/motherboard/accelerator RAM, ROMs, WCS), `ram_init` |
| `CIAA`, `CIAB` | bus fields | `cia_a`, `cia_b` |
| `PAUL` | bus fields | `paula` |
| `AGNS` | bus fields | `agnus` |
| `COPR` | bus fields | `copper` |
| `DENI` | bus fields | `denise`, `denise_revision` |
| `BLIT` | bus fields | `blitter` |
| `FLOP` | bus fields | `floppy` (controller, drives, in-memory disk images) |
| `RTC ` | bus fields | `rtc`, `rtc_present` |
| `GAYL` | bus fields | `gayle` |
| `MOBO` | bus fields | `ramsey`, `gary`, `sdmac`, `ide_a4000` |
| `UAEL` | bus fields | `uaelib` |
| `CART` | bus fields | `cartridge` |
| `AKIK` | bus fields | `akiko` |
| `CDTV` | bus fields | `cdtv` |
| `ZORR` | bus fields | `devices` (every `BoardDevice`) |
| `KEYB` | bus fields | `keyboard` (the 6500/1 MCU model) |
| `INPT` | bus fields | `input` (controller ports) |
| `BUS ` | bus rest | every other `Bus` field: DMA arbitration, interrupt latches, beam-event capture, presentation windows, diagnostics |

`Bus` keeps a single derived `Serialize`/`Deserialize`; the split is done
by two serde adapters in `savestate/split.rs`. `BusSplitter` is a
`Serializer` that only accepts a struct and streams each field into the
chunk that claims it (`savestate/chunk.rs` holds the table): a chunk
opens when its first field arrives and closes when a field of another
chunk does, so a chunk's fields must be adjacent in `Bus` (the splitter
refuses a chunk that closes short), and only the catch-all `BUS ` chunk,
whose fields are scattered through the struct, is buffered until the end.
A field added to `Bus` lands there with no table edit. `BusJoiner` is the
`Deserializer` counterpart: it pulls chunks from the stream as the derived
visitor asks for the next field, decoding each `Bus` chunk's field map
straight from the stream and handing the value chunks to the machine, in
whatever order the chunks appear, so `Bus::deserialize` sees what a
single-map encoding would have given it. Each `bus fields` payload is
therefore a map from field name to value; a `value` payload is the value
itself.

Neither side holds a copy of the state: saving keeps at most one block of
the chunk being written plus the catch-all buffer (a few MiB of glue), and
loading keeps at most one block of the chunk being read plus what has been
decoded so far. A machine with gigabytes of memory-backed disk images
therefore saves and loads with the same footprint the flat format had.

Chunk payloads are MessagePack in a fixed dialect (`chunk::encode`):

- structs as maps keyed by field name; enum variants by name (a unit
  variant is its name as a string, a variant with data is a one-entry map
  `{name: data}`);
- `Vec<u8>` and byte slices as MessagePack `bin` (the `rmp-serde`
  `ForceIterables` bytes mode), so RAM, ROM, and disk images are stored
  as raw bytes; other sequences and fixed arrays (including the
  `serde-big-array` ones) as arrays;
- integers at their natural MessagePack width, so a field widened from
  `u8` to `u32` still reads its old values; `Option` as `nil` or the
  value; `HashMap`/`BTreeMap` as maps.
- `BoardDevice` keeps its custom encoding as a two-element array of an
  explicit `u32` kind and the board payload (`zorro_device/state.rs`).
  IDs stay reserved when their Cargo feature is disabled, and loading an
  unsupported kind reports the missing board. Existing IDs must never be
  reused or renumbered.

A `Bus` chunk decoded from the stream runs under the decoder's own
nesting limit (64 levels), and its framing must end exactly with its last
field. A payload that is decoded from memory (the descriptor, the value
chunks, and any chunk a migration rewrites, whose value-tree decoder has
no limit of its own) is first walked by `chunk::check_shape` without
building anything: it must be exactly one well-formed value that ends at
the end of the payload and nests no deeper than 64 levels. So a crafted
payload cannot drive a recursive decoder off the stack. Payload buffers
grow one MiB at a time as bytes arrive, so a header or block claiming more
than the stream holds fails at the end of the stream having allocated no
more than one MiB past what exists; a state's legitimate size is unbounded
by design (memory-backed disk images ride in the payload), so there is
deliberately no fixed cap.

Compression is `flate2` at `Compression::fast()`; any standard zlib
inflater reads it regardless of level.

Container versions 1 through 80 were a different layout: the descriptor
and five bincode components (`CpuCore`, `MachineRuntimeState`, the two
caches, the whole `Bus`) written positionally, with no field names and
one global version. Those files are refused with a message saying the
state must be saved again from a current build; there is no reader for
them.

In-process snapshots do not use this container. `M68kMachine::write_state`
writes the same five components as unframed positional bincode for the
reverse-debugging ring, run-ahead, and netplay rollback checkpoints
(`emulator.rs`), where speed matters and only the build that wrote a blob
ever reads it. `write_chunks` is its file-format twin; both go through
the same `adopt_state` tail on restore.

## Versioning

Three things are versioned, at three rates:

- **The container** (`STATE_VERSION`): the framing above. It moves only
  when the framing changes, and a mismatch is refused outright.
- **Each chunk** (`chunk::CHUNKS`): the meaning of one subsystem's
  payload. A chunk written at an older version than the build reads is
  upgraded on load by the `chunk::Migration` steps registered for it
  (`chunk::MIGRATIONS`), one version at a time, as a rewrite of the
  decoded value tree, before the payload is decoded into the live structs.
  A missing step fails the load naming the chunk and both versions; a
  chunk from a newer build is refused as such.
- **Nothing**, for most struct changes. Because payloads name their
  fields, the derived deserializers tolerate a field the file has that the
  struct no longer has (ignored), a field the struct has that the file
  lacks when it is an `Option` or carries `#[serde(default)]` (defaulted),
  reordered fields, and widened integers. Unknown chunks are skipped, so a
  build may add a chunk without invalidating its states for older builds.

The rule for a change to a serialized struct is therefore:

1. Adding a field: give it `#[serde(default)]` (or `#[serde(default =
   "fn")]`, or make it an `Option`) when the default is the right reading
   of a state that predates it. No version changes. Without a default the
   field is required and every older state fails to load naming the chunk
   and field, which is only acceptable while the format is on a
   development branch.
2. Removing or reordering a field: nothing to do.
3. Renaming a field whose old value must carry over: `#[serde(alias =
   "old_name")]`.
4. Changing what a field means, or its representation in a way the
   MessagePack reader cannot bridge (an integer that became a struct, a
   tuple whose elements changed roles, a widened enum whose old variants
   were renumbered): bump the owning chunk's version in `chunk::CHUNKS`,
   note it in the version comment there, and add a `Migration` for the
   previous version that rewrites the old shape. Add a test that loads a
   payload of the old shape through it.
5. Adding a subsystem: give it a chunk (table entry naming its `Bus`
   fields, which must sit next to each other in `Bus`) rather than letting
   it grow the `BUS ` catch-all.

`savestate::SCHEMA_FINGERPRINT` hashes the crate version, the container
version, and every chunk's tag and version; the netplay handshake
(`netplay/wire.rs`) carries it so two peers agree they serialize a machine
identically before trusting each other's frame checksums.

`savestate::tests` cover each of these paths against a real machine:
unknown chunks and reordered chunks
(`unknown_chunks_are_skipped_and_chunk_order_does_not_matter`), fields a
struct no longer has or did not have yet
(`states_survive_fields_a_struct_no_longer_has_or_did_not_have_yet`),
migrations and newer-chunk refusal
(`older_chunk_versions_load_through_migrations_and_newer_ones_are_refused`),
the table naming real `Bus` fields
(`every_bus_chunk_holds_exactly_the_fields_it_claims`), the end marker and
trailer checks (`the_end_marker_and_the_compressed_trailer_are_verified`),
and hostile lengths and nesting
(`hostile_chunk_lengths_and_nesting_fail_instead_of_exhausting_memory`).

## Snapshot point and atomicity

File saves write through a buffered compressor into a unique sibling temporary
file. After compression and flushing succeed, the file is synced and atomically
renamed over the destination. Returned errors leave an existing save untouched
and remove the temporary file. This guarantees complete-file replacement, not
power-loss durability of the directory entry: the containing directory is not
fsynced. The browser uses `save_to_writer` and manages publication itself.

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

The regression checks cover serialization, failure recovery and replay:

- `cpu::tests::save_state_round_trip_replays_identically`: runs a
  chip-RAM loop that also writes COLOR00 (so CPU, RAM, and beam-event
  capture all advance), saves at T1, runs 20k instructions to T2 saving
  the state again, rewinds to T1, replays the same step pattern, and
  asserts the trace matches **and the re-serialized T2 state file is
  byte-identical** to the original timeline's.
- `zorro_device::state::tests` read and reproduce the same checked-in board
  fixture in default, core-only and MHI-only builds. Disabled kinds also have
  explicit error coverage.
- `savestate::tests` cover failed writes and failed publication preserving
  existing destinations and cleaning up temporary files, magic/version
  rejection (including the flat pre-81 layout), the truncated-payload
  atomicity guarantee, the header descriptor round trip
  (`round_trips_the_machine_descriptor`), the chunk directory a file
  presents (`state_file_is_a_directory_of_versioned_subsystem_chunks`),
  the split/join adapters on their own
  (`splitter_streams_struct_fields_by_chunk_and_joiner_reassembles_them`),
  the compatibility paths listed under [Versioning](#versioning), and
  that a CD controller travels in the state so the bar's CD controls
  appear on load (`cd_controller_travels_in_the_state`);
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

Reverse debugging keeps a ring of recent machine states. To reach an earlier
point, it restores the nearest preceding snapshot and replays forward.
The user-facing controls (headless `COPPERLINE_DBG_RWATCH`, the
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
count bumped per retired instruction in `execute_cpu_slice`. It is stored
alongside each snapshot in the ring, outside the serialized machine blob.
A reverse step to position *P* restores the nearest
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

(determinism-boundaries)=
### Determinism boundaries

The same host boundary as save states applies, plus the requirement that
*time-dependent* inputs be pinned, since replay re-executes them:

- A fitted RTC reads host wall-clock time unless seeded with `--rtc-time`
  or fixed with `COPPERLINE_RTC_FIXED_SECS`. The headless reverse-mode
  warning checks only the environment override, so it can still appear
  when `--rtc-time` already makes the clock deterministic.
- Directory-backed (host-folder) filesystems stamp guest-visible host
  datestamps with no fixed-time override -- avoid for reverse replay.
- HDF/CD images reopen by path and are externally mutable, so a guest disk
  write after a snapshot is not rolled back by restoring it; floppy
  contents are in-state and safe.
- Physical media and live network, serial, MIDI, or sampler input cannot
  be reproduced from the snapshot alone.

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


(winuae-interchange)=
## WinUAE interchange (`uss.rs`)

USS import is separate from native serialization and does not change
`STATE_VERSION`. `UssFile` parses and validates the complete ASF chunk stream
before installing RAM, CPU registers and hardware latches in a newly built
machine. Lengths include the 12-byte chunk header; bit 0 requests zlib with
a big-endian inflated length prefix. Padding is `4 - (payload_length % 4)`,
including four bytes for an aligned chunk. END has an eight-byte header.
Input and total inflated bytes are limited to 256 MiB, individual chunks to
128 MiB, and the stream to 4096 chunks.

The compact CHIP layout omits audio and sprite register blocks, which are
restored from AUD0-3 and SPR0-7. Custom writes skip command/trigger registers
such as BLTSIZE, COPJMP and DSKLEN; importing them as ordinary writes would
start transfers absent from the snapshot. CIA counters use big-endian words
while their saved latches and TOD bytes use little-endian ordering. ROM
matching uses the declared CRC over the normalized 256/512 KiB image,
accepting a mirrored 256 KiB ROM and naming a mismatch through `romdb`.
The validated CHPX flag word restores the live boot-ROM overlay at address
zero; absent CHPX data retains the older chip-RAM mapping convention.

WinUAE's event queue, CPU prefetch/cache contents, Copper phase and shift
register pipelines have no direct Copperline representation. Import starts
at a reconstructed beam boundary, restarts Copper from COP1LC, and advances
one discarded frame through the normal CPU/chipset path. This is an
interchange approximation, not byte-identical resumption of WinUAE timing.
Unsupported active blitter/disk operations and device chunks are rejected;
other omitted chunks are reported. See the
[coverage assessment](../guide/winuae-state.md#coverage) before extending it.

The Bartman binary profile writer (`profile/bartman.rs`) is a driver-owned
bounded operation shared by headless and GUI GDB and the offline CLI. It
uses the normal precise CPU sampler and full bus trace, translates wire
record fields explicitly, and embeds the real framebuffer. Its temporary
file, progress transport and instrumentation are host state and never enter
native snapshots.

Windowed Bartman `--run` sessions start paused before the first GDB client
connects. The initial stop query then drives the shared core's LoadSeg
handshake and saves the program-entry state used by `monitor reset`.
This keeps debugger startup latency from consuming the load event before
the per-connection library tracker is armed. Other GUI GDB launches retain
their normal run-until-attach behavior.


An externally forced debugger PC write invalidates the instruction prefetch
queue before execution resumes at the new address. Native snapshot restores
retain their saved queue for exact replay; USS imports start with a cold
queue at the imported PC.
