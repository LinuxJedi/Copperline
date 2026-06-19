# The headless debugger

A second, scriptable debugger (`src/debugger.rs`) is driven entirely
by `COPPERLINE_DBG_*` environment variables and works in any run, including
windowless `--screenshot-after` / `--dump-frames` captures. It is the main
tool for timing and compatibility investigations: because the core is
deterministic, a failing run can be replayed with progressively more
instrumentation and every replay hits the same cycle.

Output goes through the `log` crate at info level, so set `RUST_LOG=info`
(or `debug`) to see it.

```sh
RUST_LOG=info \
COPPERLINE_DBG_BREAK=C033C2 \
COPPERLINE_DBG_DUMP=C09580:4 \
COPPERLINE_DBG_SHOT=/tmp/hit \
./target/release/copperline --config copperline.example.toml --noaudio \
  --screenshot-after 30 /tmp/out.png
```

All addresses are hexadecimal, with or without a `0x` prefix. Like every
`COPPERLINE_*` knob, the variables are snapshotted once at startup and
cannot change at runtime (see [](../internals/architecture)).

## Variables

`COPPERLINE_DBG_BREAK=PC[,PC...]`
: PC breakpoints. Each hit logs a `DBG BREAK` report: emulated time, frame,
  beam position (`v=`/`h=`), SR, PC, the full register file, any
  `DBG_DUMP` memory regions, and a screenshot if `DBG_SHOT` is set.

`COPPERLINE_DBG_WATCH=ADDR[:LEN][,...]`
: Memory watchpoints (LEN in bytes, default 2). Logs when a watched word
  changes, whoever wrote it:
  `DBG WATCH 0x00c09580 0012->0013 by pc=0x00c03374`.

`COPPERLINE_DBG_DUMP=ADDR:WORDS[,...]`
: Memory regions to hex-dump with every break/watch report
  (`mem 0x00c09580: 0000 0001 0002 0003`).

`COPPERLINE_DBG_TRACE=1`
: Disassembled per-instruction trace while the debugger window (AFTER/UNTIL)
  is active, with key registers on each line. Capped at 200,000 lines per
  run as a flood guard.

`COPPERLINE_DBG_TRACE_FULL=1`
: Like `TRACE`, but each line is a fixed-width, all-hex record of the entire
  register file (`D0`-`D7`/`A0`-`A7`) and the CCR, prefixed `ft`. Intended for
  diffing Copperline's instruction stream against a reference 68000 (e.g.
  vAmiga) to isolate a mis-emulated instruction. Implies `TRACE`.

`COPPERLINE_DBG_TRACE_LO=ADDR` / `COPPERLINE_DBG_TRACE_HI=ADDR`
: Restrict the trace to instructions whose PC is in `[LO, HI]`. This isolates a
  single routine (e.g. a depacker loop) and, by excluding interrupt handlers,
  yields a contiguous deterministic stream that lines up across emulators.

`COPPERLINE_DBG_RAMDUMP=ADDR:LEN:FILE`
: One-shot memory dump the first time the debugger activates: LEN bytes
  from hex address ADDR are written to FILE, read through the CPU's own
  memory decode so chip-RAM mirrors resolve. Combined with AFTER, this
  captures bitplane or sample data exactly as displayed at a moment in
  time for offline analysis.

`COPPERLINE_DBG_COPPER=auto | ADDR[:COUNT]`
: One-shot Copper-list disassembly the first time the debugger activates.
  `auto` reads the live COP1LC; an explicit address disassembles from
  there. COUNT defaults to 256 instructions (`auto:64` works too).

`COPPERLINE_DBG_AFTER=SECS` / `COPPERLINE_DBG_UNTIL=SECS`
: Activity window in emulated seconds. Outside the window the debugger is
  inert, which keeps traces focused and runs fast: combined with
  determinism, you can binary-search a failure in time.

`COPPERLINE_DBG_MAXHITS=N`
: Stop reporting after N hits (default 200).

`COPPERLINE_DBG_SHOT=PREFIX`
: Save a PNG of the last completed frame on every hit, as
  `PREFIX-0000.png`, `PREFIX-0001.png`, ...

## Diagnostic knobs

Beyond the debugger, many subsystems have start-up diagnostic switches.
They are read through `src/envcfg.rs`; grep its call sites for the
authoritative list. The most useful ones:

| Variable | What it logs / does |
|---|---|
| `COPPERLINE_DIAG_SLOTMAP` | Per-colour-clock chip-bus owner map for a frame (`R`efresh, `B`itplane, `S`prite, `D`isk, `A`udio, `C`opper, b`L`itter, c`P`u, `.` idle); `COPPERLINE_DIAG_SLOTMAP_AT=SECS` picks the frame |
| `COPPERLINE_DIAG_IPL` | CPU cycles spent per interrupt level |
| `COPPERLINE_DIAG_PCSAMPLE` | Top-50 executed-PC histogram every 50 frames |
| `COPPERLINE_DIAG_PCHIST` | Full PC history (with `COPPERLINE_DIAG_PCHIST_START=SECS`) |
| `COPPERLINE_DIAG_COPLEN` | Copper list length (optionally at a given emulated time) |
| `COPPERLINE_DIAG_DISPLAY` | Display-register change log |
| `COPPERLINE_DIAG_CAPROW` | `=all`, `=V`, or `=START:END`: per-line bitplane capture state at DDF start, including DMACON, current and DDF-anchor BPLCON0, FMODE/DIW/DDF, effective fetch window, unit/period/quantum, words/row, modulos, and all BPLxPTs -- separates wrong-pointer from wrong-decode display bugs |
| `COPPERLINE_DIAG_PALETTE_ROW` | `=all`, `=V`, or `=START:END`: log beam-timed COLOR writes for selected beam lines, including source, framebuffer x, palette entry, LOCT, value, and BPLCON3; the setting is cached after first use |
| `COPPERLINE_DIAG_SPRITES` | Sprite DMA fetch/render log |
| `COPPERLINE_DIAG_SPRCAP` | `=BEAMY` or `=all`: log every captured sprite DMA line (frame, channel, hstart, attach, FMODE width, data words) on one beam line or all of them; also logs SPRxPT retargets of latched sprite descriptors |
| `COPPERLINE_DIAG_MANUAL_SPRITES` | `=BEAMY` or `=all`: log manually replayed sprite intervals, sprite register writes, BPLCON3/BPLCON4/FMODE/COLOR timing, sprite pointer alignment, and held wide-sprite words |
| `COPPERLINE_DIAG_SPRITE_PIXELS` | `=BEAMY[,STEP]`: sample non-transparent sprite pixels on one beam line, including sprite or attached-pair index, palette entry, RGB, BPLCON3, and BPLCON4; STEP defaults to 32 framebuffer pixels |
| `COPPERLINE_DIAG_BLITREGS` | `=START:END` (emulated seconds): log the full blitter register set at every blit start (classic BLTSIZE and ECS BLTSIZH); pairs with `COPPERLINE_DUMP_BLITMEM` snapshots for offline blit verification |
| `COPPERLINE_TRACE_BLITTER` | Path to a JSONL trace of blitter starts, forced finishes, DMACONR polls, and completion IRQ latches; start records include minterm/control registers, DMA/display context, FMODE, and all eight bitplane pointers |
| `COPPERLINE_DIAG_DISK` | Disk DMA state changes (DSKLEN writes) |
| `COPPERLINE_DIAG_AUDIO_NOTES` | Paula channel note on/off events |
| `COPPERLINE_DIAG_CRASH` | CPU exception/halt conditions |
| `COPPERLINE_DIAG_GAYLE` / `COPPERLINE_DIAG_CDTV` | Gayle IDE / CDTV controller traffic |
| `COPPERLINE_DIAG_A2091` | A2091 SCSI board register traffic (DMAC + WD33C93 accesses; the trace that brings up boot-ROM issues) |
| `COPPERLINE_DUMP_BLITMEM=START:END:LO:HI` | Dump chip RAM `[LO,HI)` on every BLTSIZE write between START and END emulated seconds; output goes to `$TMPDIR/copperline-blitdump` unless `COPPERLINE_DUMP_BLITMEM_DIR` is set |
| `COPPERLINE_DUMP_BUS_ACCOUNTING` | Per-frame chip-bus slot accounting |
| `COPPERLINE_DUMP_RENDER_META[_VERBOSE]` | Renderer event/fetch metadata |

Timing-model knobs that pair well with the debugger:

- `COPPERLINE_IRQ_LATENCY_CCK=N` -- override the modelled 68000
  interrupt-recognition latency (default 65 colour clocks; `0` disables).
- `COPPERLINE_DBG_AFTER=SECS` / `COPPERLINE_DBG_UNTIL=SECS` -- bound
  debugger and renderer diagnostics to an emulated-time window. Renderer
  diagnostics parse these bounds once when their diagnostic option is first
  used.
- `COPPERLINE_HCENTER=0` -- disable presentation recentring when debugging
  display alignment.
- `COPPERLINE_SHOT_RAW=1` -- save screenshots as the raw 716x570 woven
  framebuffer instead of the 4:3 presentation scale. The presentation
  resampler blends adjacent lines, so per-scanline forensics (which exact
  framebuffer row carries an artifact) need the raw field.
- `COPPERLINE_OVERSCAN=full|tv` -- override the configured overscan mask.
- `COPPERLINE_DEINTERLACE=0` -- disable the motion-adaptive deinterlacer.
- `COPPERLINE_PHOSPHOR=0.0..0.95` -- CRT phosphor persistence for one run
  (overrides `[display] phosphor`).
- `COPPERLINE_THREADED_RENDER=0` -- force the synchronous renderer instead
  of the default render worker when bisecting presentation or capture
  issues.
- `COPPERLINE_REAL_PACING_BUDGET=cycles|instructions` and
  `COPPERLINE_REAL_CPU_CPI=N` -- pacing-budget overrides (see
  [](../internals/timing)).
- `COPPERLINE_AUDIO_PROFILE=1` / `COPPERLINE_REAL_PACING_PROFILE=1` --
  one-line-per-second performance counters (see [](../internals/peripherals)).

Behavior-changing A/B switches such as `COPPERLINE_NO_*`,
`COPPERLINE_EXP_*`, `COPPERLINE_DISK_SPEED_DIV`, and
`COPPERLINE_DBG_EXTCCK` are compiled only with the
`internal-diagnostics` feature. Normal builds ignore them so release runs
stay hardware-derived and reproducible.

## A worked example

A frame-pacing investigation is a template for using these tools
together:

1. Reproduce headlessly: `--screenshot-after` at a known-bad timestamp.
2. Find the guest's frame pacing: `COPPERLINE_DIAG_PCSAMPLE` to locate
   the hot loop, then `COPPERLINE_DBG_BREAK` on the loop head with
   `COPPERLINE_DBG_DUMP` of its counters.
3. Narrow in time with `COPPERLINE_DBG_AFTER`/`UNTIL`, watch the
   interesting word with `COPPERLINE_DBG_WATCH`.
4. Check the bus: `COPPERLINE_DIAG_SLOTMAP_AT` to see who owned every
   colour clock of the suspect frame.
5. Compare against real hardware with the `timing-test/` disk when the
   question is "is this operation too fast/slow".
