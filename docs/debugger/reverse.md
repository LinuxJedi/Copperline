# Reverse debugging

Copperline can step *backwards*. Because the core is deterministic and
already snapshots the whole machine (see [](../internals/savestate)), going
back in time needs no special record layer the way a tool like `rr` does:
the emulator keeps a ring of periodic in-memory snapshots, and to reach any
earlier point it restores the nearest snapshot at or before it and replays
forward. The reconstruction is byte-identical as long as the determinism
preconditions below hold.

The same machinery backs two surfaces: a headless "last writer" reverse
watchpoint for automated root-cause hunts, and **&lt; Step** / **&lt; Run**
controls in the [debugger window](window).

## What it is good for

The recurring hard question in a timing or corruption investigation is
"*the value at address X is wrong by the time I look -- which instruction
produced it?*". Forward watchpoints ([](headless)) tell you about writes as
they happen, but you have to know to watch before the damage. A reverse
watchpoint answers the question after the fact: stop at the symptom, then
ask for the last writer.

## Headless: the "last writer" reverse watchpoint

Set `COPPERLINE_DBG_RWATCH` to an address and `COPPERLINE_DBG_UNTIL` to the
emulated time to evaluate it at. When the run reaches that time, the
emulator restores recent snapshots, replays with a watch on the word, and
logs the last instruction that changed it -- then restores the live state
and continues, so the run (and any `--screenshot-after`) is unaffected.

```sh
RUST_LOG=info \
COPPERLINE_RTC_FIXED_SECS=1000000000 \
COPPERLINE_DBG_RWATCH=DE488 \
COPPERLINE_DBG_UNTIL=12.5 \
./target/release/copperline --config X.example.toml --noaudio \
  --screenshot-after 13 /tmp/out.png
```

```text
DBG RWATCH last writer of $0DE488: CAFE->0000 by pc=0x00FA37D8 pos=561401 f=40 cck=2864664
```

The report gives the writing instruction's PC, the value transition, and
the position/frame/colour-clock of the write. The credited PC follows the
same rule as the forward `COPPERLINE_DBG_WATCH`: a write made by the Copper
or blitter between two instructions lands on the next CPU instruction's PC.

### Variables

`COPPERLINE_DBG_RWATCH=ADDR[:LEN]`
: Arms the reverse watchpoint on the word at `ADDR`. Evaluated at
  `COPPERLINE_DBG_UNTIL`, or at run end if that is unset.

`COPPERLINE_DBG_RR=1`
: Arms the snapshot ring without a watchpoint, so reverse-step navigation
  has history to work from (useful when driving the window).

`COPPERLINE_DBG_RR_BUDGET_MB=N`
: Snapshot-ring memory cap in MiB (default 512). The oldest snapshots are
  evicted once the total exceeds it; a query for a point older than the
  retained history reports `beyond retained snapshot history`.

`COPPERLINE_DBG_RR_INTERVAL=N`
: Emulated frames between snapshots (default 5). Smaller means shorter
  replays (faster reverse ops) but more memory and more forward-run
  serialization overhead.

## In the window

Opening the debugger arms the ring automatically (at a conservatively large
snapshot interval, since captures only accrue while the machine advances --
**Run** or **Frame**, not while paused). Two reverse controls then sit at
the right of the transport row:

| Control | Effect |
|---|---|
| **&lt; Step** | Step one instruction backward |
| **&lt; Run** | Run backward to the previous PC breakpoint hit |

The status line shows the current instruction position and how much history
is retained (`pos N  rev K snaps, M MB`). **&lt; Run** uses the PC
breakpoints set on the Break tab; watch-based reverse-continue is not yet
modelled.

## Determinism preconditions

Reverse replay is exact only if everything that fed the original timeline is
reproduced. When reverse mode is armed the emulator logs a warning if the
first of these is unmet:

- **RTC**: set `COPPERLINE_RTC_FIXED_SECS`. Otherwise the guest's real-time
  clock reads host wall-clock time, which differs on replay and diverges.
- **Input** (keyboard, mouse, joystick) is recorded as it is applied and
  re-applied during replay, so scripted (`--script`, `--press-after`, ...)
  and live window input both reconstruct. A floppy **media change** inside a
  replayed interval cannot be reconstructed (the inserted image is host-file
  state) and is reported with a warning.
- **Hard-drive / CD** images are reopened by path and are externally
  mutable, so a guest disk write after a snapshot is not rolled back by
  restoring it. Floppy contents are part of the snapshot and are safe. Most
  demo/root-cause targets are RAM-only and unaffected.
- Replay must run with the **same machine config and `COPPERLINE_*`
  environment** as the forward run (the environment is snapshotted once at
  startup; pacing/clock knobs change the seconds-to-instruction mapping).

See [](../internals/savestate) for the snapshot-ring and replay model.
