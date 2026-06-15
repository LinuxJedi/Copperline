# Headless and scripted runs

The emulated core is deterministic and independent of wall-clock pacing, so
the preferred way to verify behaviour -- in CI, in regression tests, or
while developing -- is a headless run: the window stays hidden, the core
runs unthrottled, and the result is reproducible.

## Screenshots

```sh
./target/release/copperline --config copperline.example.toml --noaudio \
  --screenshot-after 30 /tmp/out-30s.png
```

`--screenshot-after SECS PATH` emulates for SECS *emulated* seconds, saves
the framebuffer as a PNG, and exits.

## Frame dumps

For frame-to-frame rendering glitches, dump consecutive rendered frames:

```sh
./target/release/copperline --config copperline.example.toml --noaudio \
  --dump-frames /tmp/frames --dump-start 24 --dump-count 120
```

Files are written as zero-padded PNGs into the directory and the emulator
exits after the requested count. `--dump-start` defaults to 0.

(save-states-headless)=
## Save states

`--save-state-after SECS PATH` writes a [save state](ui.md#save-states) of
the whole machine at SECS emulated seconds and keeps running;
`--load-state PATH` restores one before the run starts, resuming from the
state's emulated timeline. Together they collapse long debug loops: pay
the boot/loading time once, then iterate from just before the scene under
investigation.

```sh
# Once: snapshot 2 minutes in, just before the scene under investigation.
./target/release/copperline --config copperline.example.toml --noaudio \
  --save-state-after 120 /tmp/snapshot-120s.clstate \
  --screenshot-after 121 /tmp/throwaway.png

# Then iterate: each run resumes at 120s and reaches 125s in seconds.
./target/release/copperline --config copperline.example.toml --noaudio \
  --load-state /tmp/snapshot-120s.clstate \
  --screenshot-after 125 /tmp/scene.png
```

The core is deterministic, so a resumed run is byte-identical to an
uninterrupted one -- screenshots from either path can be compared
directly. Scripted-input timestamps (below) are absolute emulated time:
after `--load-state` of a 120s state, a `--press-after 60 ...` has
already passed and fires immediately, and a `--press-after 130 ...`
fires 10 seconds in. Save states do not embed hard-drive or CD image
file contents; they are reopened from their original paths.

## Scripted input

Input can be scheduled at emulated timestamps, which composes with
screenshots and frame dumps to drive menus, trainers, and loaders
deterministically:

| Flag | Effect |
|---|---|
| `--press-after SECS KEY` | Press and release an Amiga key (default ~100 ms hold) |
| `--key-after SECS KEY MS` | Hold a key for exactly MS milliseconds (for modifier chords) |
| `--click-after SECS BUTTON MS` | Press a mouse button (`left`/`right`/`middle`) for MS milliseconds |
| `--joy-after SECS BUTTON MS` | Press a port-2 joystick / CD32-pad control (`up`/`down`/`left`/`right`/`red` (alias `fire`)/`blue`/`green`/`yellow`/`play`/`rwd`/`ffw`) for MS milliseconds |
| `--mouse-after SECS DX DY` | Apply a relative port-1 mouse motion of (DX, DY) counter steps |
| `--insert-disk-after SECS DFN PATH` | Insert a disk image into `df0`..`df3` |
| `--defer-disk-insert SECS DFN` | Start with the configured drive empty, then insert its configured image |
| `--script FILE` | Run scripted-input directives from a file (below) |
| `--record-input PATH` | Record all machine-bound input for the whole run; the script is written to PATH on exit |

`KEY` is an Amiga raw key code (`0x45`, decimal also accepted) or a name:
`ctrl`, `lalt`, `lami`, `f1`, `esc`, `left`, letter and digit keys, and so
on. All the flags repeat, so several inputs can be queued:

```sh
./target/release/copperline --key-after 14.0 ctrl 500 --press-after 14.1 c
```

## Input recording and script files

Long input sequences live in a script file instead of the command line:
one directive per line in the flag syntax without the leading dashes,
with `#` comments, blank lines, and double-quoted paths allowed. Only the
scripted-input directives are accepted -- a typo cannot silently change
emulator configuration.

```
# drive a loader prompt, then start
joy-after 60.0 red 300
key-after 75.0 f1 200
insert-disk-after 90.0 df1 "disk 2.adf"
```

Run it with `--script FILE` (combines freely with the other flags).

Rather than writing scripts by hand, record one: in the window,
`Cmd+Shift+R` on macOS or `Alt+Shift+R` on Linux/Windows starts and stops a
live-input recording, written to
`copperline-input-<unix-ts>.clscript` in the working directory; the
headless equivalent `--record-input PATH` records the whole run and
writes the file on exit. Every input event that reaches the emulated
machine is captured with its emulated timestamp -- key holds, mouse
buttons and motion, port-2 joystick / CD32-pad controls, and floppy
inserts -- so a manually driven session replays deterministically:

```sh
# Play through the section by hand once...
./target/release/copperline --config copperline.example.toml \
  --record-input /tmp/session.clscript

# ...then replay it headlessly with the same emulated inputs.
./target/release/copperline --config copperline.example.toml --noaudio \
  --script /tmp/session.clscript --screenshot-after 60 /tmp/check.png
```

Recorded times are absolute emulated seconds, which makes recordings
compose with save states: `--load-state` of a snapshot plus the script
recorded from that point is a complete, shareable reproduction. Mouse
motion is captured at frame granularity (one `mouse-after` per frame of
movement); CD inserts are not recorded -- use the `[cd]` config section
for those.

## Audio capture

- `--noaudio` runs silent (live audio is otherwise on by default).
- `--audio-wav PATH` writes the mixed stereo output as a 32-bit float
  44.1 kHz WAV in emulated time instead of playing it -- useful for
  comparing audio behaviour across runs.
- `--profile-live-audio SECS` runs a windowless Paula-to-cpal profiling
  workload; combine with `COPPERLINE_AUDIO_PROFILE=1` for live-audio
  counters (see [](../internals/peripherals)).

## Investigating a run

The [headless debugger](../debugger/headless) layers on top of any of
these runs through `COPPERLINE_DBG_*` environment variables: breakpoints,
watchpoints, instruction traces, Copper-list dumps, and per-hit screenshots,
all without a window.

```sh
RUST_LOG=info \
COPPERLINE_DBG_BREAK=C033C2 COPPERLINE_DBG_DUMP=C09580:4 \
./target/release/copperline --config copperline.example.toml --noaudio \
  --screenshot-after 30 /tmp/out.png
```

## The vAmigaTS compatibility suite

An ignored integration test runs ADFs from a local
[vAmigaTS](https://github.com/dirkwhoffmann/vAmigaTS) checkout through
Copperline with Kickstart 1.3 in DF0:, captures screenshots after the
suite's default 9-second wait, and can compare them against a baseline
directory or a vAmiga reference render:

```sh
COPPERLINE_VAMIGATS_DIR=/path/to/vAmigaTS \
COPPERLINE_VAMIGATS_KICK13=/path/to/kick13.rom \
COPPERLINE_VAMIGATS_FILTER=bbusy0 \
cargo test --release --test vamiga_ts -- --ignored --nocapture
```

Optional variables: `COPPERLINE_VAMIGATS_LIMIT=N` (cap test count),
`COPPERLINE_VAMIGATS_SECONDS=SECS` (screenshot delay),
`COPPERLINE_VAMIGATS_OUT=DIR` (keep generated configs and screenshots),
`COPPERLINE_VAMIGATS_BASELINE=DIR` (require PNGs to match a baseline), and
`COPPERLINE_VAMIGATS_VAMIGA=/path/to/vAmiga` plus
`COPPERLINE_VAMIGATS_VAMIGA_SETUP=NAME` (render vAmiga references via its
RetroShell regression path).
