# The window, status bar, and menus

Copperline opens a single window: the emulated display presented at a
TV-like 4:3 aspect ratio, above a status bar with the machine's controls.
The window scales continuously when resized.

## Keyboard shortcuts

The app shortcut modifier is `Cmd` on macOS and `Alt` on Linux/Windows.

| macOS | Linux/Windows | Action |
|---|---|---|
| `Cmd+Q` | `Alt+Q` | Quit |
| `Cmd+S` | `Alt+S` | Save a screenshot (`copperline-screenshot-<YYYYMMDDHHmmSS>.png` in the working directory; the on-screen confirmation overlay is not part of the saved image) |
| `Cmd+R` | `Alt+R` | Start / stop a video-with-audio recording (below) |
| `Cmd+Shift+R` | `Alt+Shift+R` | Start / stop an input recording (below) |
| `Cmd+Shift+S` | `Alt+Shift+S` | Save a state (`copperline-state-<YYYYMMDDHHmmSS>.clstate` in the working directory) |
| `Cmd+Shift+L` | `Alt+Shift+L` | Load a save state from a file dialog |
| `Cmd+D` | `Alt+D` | Swap to the next disk in a drive's configured playlist |
| `Cmd+G` | `Alt+G` | Capture / release the host mouse (clicking the display also captures) |
| `Cmd+B` | `Alt+B` | Open the [debugger window](../debugger/window) |
| `Cmd+J` | `Alt+J` | Cycle joystick input mode: auto, keyboard, gamepad |
| `Cmd+W` | `Alt+W` | Toggle Warp Speed (turbo) on / off |
| `Cmd+Shift+W` | `Alt+Shift+W` | Cycle the Warp Speed limit: 2x, 4x, 8x, 16x, Max |
| `Esc` | `Esc` | Close an open menu, tool window, or overlay panel; otherwise passed through to the Amiga |
| `Ctrl+Amiga+Amiga` | `Ctrl+Amiga+Amiga` | Keyboard reset (warm reboot) |

Host modifiers that are passed through to the emulated keyboard map onto
the Amiga keyboard: Alt becomes Amiga Alt, Cmd/Super becomes the left/right
Amiga keys, and Ctrl becomes Amiga Ctrl, so `Ctrl+Amiga+Amiga` is typed
naturally.

All other keys are sent to the emulated machine through the real path: a
bit-timed keyboard-MCU model clocks each transition into CIA-A's serial
register over the emulated KCLK/KDAT lines, with the real handshake,
power-up stream, and recovery protocol -- so even software that talks to
the keyboard hardware directly behaves. `Ctrl+Amiga+Amiga` runs the
authentic reset protocol (reset warning, then KCLK held low), so the
reboot lands a fraction of a second after the chord, as on real hardware.

## Status bar

The status bar (44 pixels below the display) holds, left to right:

- **LED block.** PWR and FDD always; a green HDD activity LED on Gayle IDE
  machines (A600/A1200); a blue CD activity LED on CDTV/CD32 that lights
  while the drive reads data or plays CD audio. A small digital counter
  shows the current floppy track.
- **Per-drive floppy controls.** Every connected drive gets a disk button
  (marked with the drive number) that opens a file dialog -- multi-select
  several images to queue a swap playlist for that drive -- plus a swap
  button that cycles to the next queued disk and an eject button. Swap and
  eject grey out when there is nothing to swap to or eject. With three or
  four drives the clusters stack two-up.
- **CD controls** on CDTV/CD32 machines: a CD button that loads (or swaps)
  a cue sheet with the proper media-change notification, and a CD eject
  button. These do not appear on machines without a CD drive.
- **Camera button**: saves a screenshot (same as `Cmd+S` on macOS or
  `Alt+S` on Linux/Windows).
- **Hamburger menu button**: opens the pop-up menu (below).
- **Volume slider**: drag, or scroll the mouse wheel over it for 5% steps.
- **Pause / power / reboot buttons.** Pause freezes emulation while staying
  powered; power cold-boots (clears RAM) or powers off back to the test
  screen; reboot is a warm reset.

## Menu, tool windows, and overlay panels

```{figure} ../images/ui-preview-menu.png
:alt: The pop-up menu
:width: 75%

The pop-up menu opened from the status bar.
```

The menu opens debugger-style tool windows and smaller overlay panels. Tool
windows are separate native windows so the emulated display remains visible;
the debugger and frame analyzer can be open at the same time. Overlay panels
are drawn over the display. While either kind is open, key presses and display
clicks stay in the UI instead of reaching the Amiga; `Esc` closes the focused
tool window or overlay.

- **Machine Configuration...**: opens the configuration screen
  ([below](#machine-configuration-screen)) to reconfigure the machine and
  relaunch it. The same screen opens automatically on a no-machine start.
- **Frame Analyzer...**: pauses the machine and opens a separate diagnostic
  window showing which chip-bus owner had each Agnus colour clock across
  the captured frame, including overscan and blanking; see
  [](../debugger/window.md#frame-analyzer-pane).
- **Debugger** (also `Cmd+B` on macOS or `Alt+B` on Linux/Windows):
  pauses the machine and opens the five-tab debugger in a tool window; see
  [](../debugger/window).
- **Calibrate Gamepad...**: the guided calibration flow, described below.
- **Joystick Input** (also `Cmd+J` / `Alt+J`): cycles between automatic
  selection, keyboard joystick emulation, and gamepad-only mode.
- **Warp Speed** (also `Cmd+W` / `Alt+W`): runs the emulator unpaced for
  fast-forward. Toggling back re-anchors real-time pacing cleanly.
- **Warp Limit** (also `Cmd+Shift+W` / `Alt+Shift+W`): cycles how fast warp
  runs. Because the window presents with vsync, emulating one frame per
  presented frame would cap warp at the host monitor's refresh rate (about
  1.2x for 50 Hz PAL on a 60 Hz display). The limit sets an output frame
  skip -- 2x, 4x, 8x, 16x, or **Max** -- so warp retires that many emulated
  frames per presented frame, making the effective speed roughly the limit
  times the refresh rate (host CPU permitting). `Max` runs flat out and
  still presents at vsync. The default is set by `[emulation] warp_speed`
  (see [Configuration](configuration.md)).
- **Record Video** (also `Cmd+R` / `Alt+R`): starts a video-with-audio
  recording; the same item (or shortcut again) stops it. See below.
- **Record Input** (also `Cmd+Shift+R` / `Alt+Shift+R`): records every
  input event that reaches the emulated machine; stopping writes a script
  file that `--script` replays deterministically. See below.
- **Save State** (also `Cmd+Shift+S` / `Alt+Shift+S`) and **Load State...**
  (also `Cmd+Shift+L` / `Alt+Shift+L`): snapshot the whole emulated machine
  to a file, or restore one and continue from exactly that point. See below.
- **Load Kickstart ROM...**: fit a different boot ROM. Pick a 512 KiB
  Kickstart, then optionally a second file for the extended ROM (512 KiB at
  $E00000 or 256 KiB at $F00000; Cancel to skip and remove any fitted
  extended ROM). The machine then cold-resets, as if the chip had been
  swapped and the power cycled.
- **Keyboard Shortcuts**: the shortcut reference.
- **About**: app version plus a summary of the emulated machine. Builds
  made from an untagged git commit append the short commit ID to the version
  shown in the window title and About panel.

```{figure} ../images/ui-preview-shortcuts.png
:alt: The keyboard shortcuts window
:width: 75%

The Keyboard Shortcuts window.
```

(machine-configuration-screen)=
## Machine configuration screen

Starting Copperline with no machine specified -- no `./copperline.toml`, no
`--config`, no ROM or override, and not a headless run -- opens the
configuration screen instead of booting. It is also available any time from the
menu's **Machine Configuration...** item, seeded with the running machine's
settings.

```{figure} ../images/ui-preview-launcher.png
:alt: The machine configuration screen
:width: 75%

The configuration screen: the machine selector across the top, category tabs
down the left, settings on the right, and the action bar along the bottom. Here
an A1200 is selected on the Memory tab; Zorro III RAM is greyed with the reason
"needs 32-bit CPU" (the A1200's 68EC020 has a 24-bit bus).
```

The layout is:

- **Machine selector** (top). Pick a machine -- A1000, A500 (OCS), A500, A500+,
  A600, A1200, CDTV, or CD32. With no profile chosen the A500 is highlighted,
  since that is the machine the defaults describe. Selecting a machine applies
  that profile's defaults (chipset, CPU, RAM, gate array, RTC) to every tab;
  settings that no longer apply (an IDE image on a non-Gayle machine, a CD image
  on a machine with no CD drive) are dropped so they cannot block a launch.
- **Category tabs** (left sidebar). *System* (chipset and Agnus/Denise
  overrides, video standard, RTC, identify board), *CPU* (model, FPU, clock,
  caches), *Memory* (chip/fast/slow/Zorro III RAM), *ROM* (Kickstart and
  extended ROM), *Floppy* (drive count and per-drive image and write-protect),
  *Hard Disk* (IDE master/slave and the A2091 SCSI ROM and units), *CD* (image,
  insert delay, CD32 NVRAM), *Zorro* (extra autoconfig boards by metadata file),
  and *A/V & Emu* (overscan, phosphor, floppy sounds and volume, power-on,
  pacing, realtime priority, warp speed, joystick input mode).
- **Settings rows** (right pane). `[<]`/`[>]` step through a value, On/Off
  buttons flip a toggle, and the **Browse** and **Clear** buttons set or remove
  a file path through a native file dialog. On the *Hard Disk* tab, once an IDE
  or SCSI drive has an image a small editable box appears next to **Browse**:
  click it and type to set the volume name for a directory mount (left blank, a
  directory mount inherits the host directory's name; the box has no effect on a
  raw HDF). A setting that does not apply to the chosen machine is greyed and
  shows why in place of its control -- "needs 32-bit CPU" for Zorro III RAM,
  "needs 68020+" for the FPU, "needs A600/A1200" for IDE.
- **Action bar** (bottom). **Load...** and **Save...** read and write a `.toml`
  config
  through a file dialog (Save writes a minimal file, only the settings that
  differ from the chosen profile's defaults, so it reads like the example
  configs). **Defaults** resets to the selected profile. **Run** validates the
  configuration and boots it; if anything is wrong -- an unusable RAM size, a
  missing disk image, an option the chosen machine cannot use -- the reason is
  shown on the status line and you stay on the screen to fix it.

Saved files use the same schema as a hand-written `copperline.toml`
(see [](configuration.md)), so the screen and the config file are
interchangeable: configure a machine and save it, or load an existing config to
tweak it. **Run** builds the machine in place, so the configuration screen and a
direct `--config` launch produce an identical machine.

## Recording video

`Cmd+R` on macOS or `Alt+R` on Linux/Windows (or the menu's "Record Video")
starts capturing the emulated display and sound to
`copperline-video-<YYYYMMDDHHmmSS>.avi` in the working directory; pressing it again
stops and finalizes the file. A red REC
badge sits in the display's top-right corner while a recording runs --
like the screenshot overlay, the badge, status bar, and menus are never
part of the captured video.

The file is an AVI with lossless ZMBV video (the DOSBox capture codec:
zlib-compressed keyframes plus frame deltas, which keeps typical Amiga
output to a few MB per minute) and uncompressed 16-bit stereo PCM audio
at 44.1 kHz. It plays directly in VLC, mpv, and anything else built on
ffmpeg; for other players, transcode with
`ffmpeg -i copperline-video-<ts>.avi out.mp4`.

Frames and audio are captured on the emulated timeline, not the host
clock: the recording stays in sync even when the host stutters, and a
capture made under Warp Speed plays back at normal speed. The audio
track is tapped before the status bar's volume slider, so recordings
keep full level regardless of the live output volume. Pausing (or
powering off) suspends the capture; recording resumes when emulation
continues.

## Recording input

`Cmd+Shift+R` on macOS or `Alt+Shift+R` on Linux/Windows (or the menu's
"Record Input") starts logging every input event that reaches the emulated
machine -- key presses with their hold times, mouse buttons and motion,
port-2 joystick / CD32-pad controls, and floppy inserts -- each stamped
with its emulated time. Pressing it again stops the recording and writes
`copperline-input-<YYYYMMDDHHmmSS>.clscript` in the working directory: a plain
text file of scripted-input directives that
`copperline --script FILE` replays exactly, because the core is
deterministic and the events re-fire at the same emulated timestamps.

This is the direct way to turn "I can reproduce it by hand" into a
regression: play the sequence once while recording, then keep the script
(optionally together with a [save state](#save-states) to skip the
lead-in) as a deterministic, shareable reproduction. The format and the
headless `--record-input` variant are described in
[](headless.md#input-recording-and-script-files).

(save-states)=
## Save states

`Cmd+Shift+S` on macOS or `Alt+Shift+S` on Linux/Windows (or the menu's
"Save State") writes a snapshot of the whole emulated machine to
`copperline-state-<YYYYMMDDHHmmSS>.clstate` in the working directory: CPU,
chip/slow/fast RAM, ROM, the full chipset and CIA state, floppy images
(including unsaved in-memory changes), expansion boards, and CD/NVRAM
state. `Cmd+Shift+L` / `Alt+Shift+L` (or "Load State...") restores one; the
machine continues from exactly the saved point, byte-for-byte -- the core
is deterministic, so a resumed run is indistinguishable from one that was
never interrupted.

States are taken at emulated-frame boundaries and are versioned: a file
from an older, incompatible build is refused with a clear message rather
than producing a corrupt machine.

A state is self-contained: it carries its own RAM, ROM, and chipset, so
loading one always restores the machine it was taken on -- even if you
launched Copperline with a different config. When the loaded machine
differs from the running one -- a different model, chipset, video
standard, RAM size, or even a different Kickstart of the same machine
(the ROM is fingerprinted) -- the load reconfigures to match the state
and tells you so (the load message names the restored machine, e.g.
"reconfigured to A1200 / 68EC020 / AGA / PAL"); your current config is
not silently mixed in. Two caveats:

- Hard-drive images (HDF files) are referenced by path, not embedded.
  The state reopens the same file on load, so guest writes made to the
  hard drive *after* the snapshot are still visible after restoring --
  treat a state as a CPU/chipset snapshot, not a disk backup. In-memory
  volumes (directory-as-HDD) and floppy images are embedded whole.
- CD images are likewise reopened by path; keep the cue/bin where it was.

The headless flags `--save-state-after SECS PATH` and `--load-state PATH`
script the same feature for [debugging workflows](headless.md): snapshot a
long-running program just before the scene under investigation once, then
iterate from the state in seconds instead of re-emulating minutes. The
file format and what exactly is (and is not) captured are specified in
[the internals chapter](../internals/savestate.md).

## Mouse and joystick

The mouse lives on port 1 and feeds the JOY0DAT counters. Click the display
(or press `Cmd+G` on macOS or `Alt+G` on Linux/Windows) to capture the host
mouse; the same shortcut releases it. While an overlay panel is open, host
cursor motion is not fed to the emulated mouse. Tool windows likewise keep
the debugger or analyzer interaction separate from Amiga mouse input.

A USB gamepad drives the emulated port-2 digital joystick: directions
through JOY1DAT, fire through /FIR1, and a second button through
POT1Y/POTGOR. On a CD32 machine the pad speaks the CD32 serial button
protocol instead, including the red/blue/green/yellow and transport
buttons. Mouse and gamepad coexist because they use different ports.

If no calibrated gamepad is connected, Copperline can emulate the port-2
joystick from the host keyboard. The joystick input mode starts at **auto**
(a calibrated USB gamepad is used when present, otherwise the keyboard
mapping is active); set a different starting mode with `[input] joystick` in
the config (or `--joystick MODE`, or the launcher's *A/V & Emu* tab) when you
want the keyboard left to the Amiga for interactive setup. Use the menu's
**Joystick Input** item, or `Cmd+J` on macOS / `Alt+J` on Linux and Windows,
to cycle through:

- **auto**: gamepad when available, keyboard fallback otherwise.
- **keyboard**: always use the keyboard mapping and ignore live gamepad
  polling.
- **gamepad**: use only a calibrated gamepad; keyboard keys pass through
  to the Amiga keyboard.

Keyboard joystick mode uses the FS-UAE-compatible mapping: cursor keys for
directions, and Right Ctrl or Right Alt for fire. For CD32 pad buttons,
`C` is red/fire, `X` is blue, `D` is green, `S` is yellow, Return is
play/pause, `Z` is rewind, and `A` is forward. While keyboard joystick mode
owns these keys, they are not sent to the Amiga keyboard.

## Gamepad calibration

Pads are read through raw `gilrs` events with no controller database, so
each controller is calibrated once: push each control when prompted. This
records raw axis/button codes and directions, which makes any pad work
regardless of database coverage and handles inverted or odd axis layouts
automatically.

```{figure} ../images/ui-preview-calibration.png
:alt: The gamepad calibration window
:width: 75%

The calibration window mid-flow. Skip covers pads without the CD32 extras.
```

Run it either from the menu ("Calibrate Gamepad...") -- which ends with a
live test of the finished bindings and a Save button that makes them live
immediately -- or from the terminal with `copperline --calibrate-gamepad`.
The steps are the four directions, fire (CD32 red), button 2 (CD32 blue),
and the optional CD32 green/yellow/play/rewind/forward buttons; every step
waits for the pad to return to neutral before sampling, so a held control
cannot bleed into the next binding.

Calibrations are saved per controller UUID in
`~/.config/copperline/gamepads.toml` (`$XDG_CONFIG_HOME` respected;
`%APPDATA%\copperline\` on Windows).
