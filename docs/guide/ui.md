# The window, status bar, and menus

Copperline opens a single window: the emulated display presented at a
TV-like 4:3 aspect ratio, above a status bar with the machine's controls.
The window scales continuously when resized.

## Keyboard shortcuts

The app shortcut modifier is `Cmd` on macOS and `Alt` on Linux/Windows.

| macOS | Linux/Windows | Action |
|---|---|---|
| `Cmd+Q` | `Alt+Q` | Quit (also the [menu's](#and-last) last row; a [calibrated gamepad's](#gamepad-calibration) optional Quit hotkey, held, quits too) |
| `Cmd+E` | `Alt+E` | Open / close the menu (also the status bar's hamburger button); releases a captured mouse |
| `Cmd+S` | `Alt+S` | Save a screenshot (`copperline-screenshot-<YYYYMMDDHHmmSS>.png` in the [screenshots folder](#where-files-go); the on-screen confirmation overlay is not part of the saved image) |
| `Cmd+R` | `Alt+R` | Start / stop a video-with-audio recording (below) |
| `Cmd+Shift+R` | `Alt+Shift+R` | Start / stop an input recording (below) |
| `Cmd+Shift+S` | `Alt+Shift+S` | Save a state (`copperline-state-<YYYYMMDDHHmmSS>.clstate` in the [states folder](#where-files-go)) |
| `Cmd+Shift+L` | `Alt+Shift+L` | Load a save state from a file dialog |
| `Cmd+1`..`Cmd+9`, `Cmd+0` | `Alt+1`..`Alt+9`, `Alt+0` | Quick-save to numbered slot 1-10 |
| `Cmd+Shift+1`..`Cmd+Shift+0` | `Alt+Shift+1`..`Alt+Shift+0` | Quick-load from that slot |
| `Cmd+D` | `Alt+D` | Swap to the next disk in a drive's configured playlist |
| `Cmd+G` | `Alt+G` | Capture / release the host mouse (clicking the display also captures) |
| `Cmd+B` | `Alt+B` | Open the [debugger window](../debugger/window) |
| `Cmd+Shift+B` | `Alt+Shift+B` | Press the freezer cartridge's button: enter the HRTMon monitor (`[cartridge] model`, see [Configuration](configuration.md#freezer-cartridge)) |
| `Cmd+K` | `Alt+K` | Open the [debugger console](../debugger/console) |
| `Cmd+J` | `Alt+J` | Toggle joystick input mode: gamepad / keyboard (also the status-bar icon) |
| `Cmd+M` | `Alt+M` | Turn the monitor bezel off, or back on to the chosen front (*Video Settings > Monitor Bezel* picks it; `[display] bezel` sets the start-up value) |
| `Cmd+Shift+A` | `Alt+Shift+A` | Cycle the audio output: Default, each host device, then Disabled (also *Audio Settings > Audio Output*) |
| `Cmd+A` | `Alt+A` | Cycle Paula's audio filter: auto, on, off (also *Audio Settings > Audio Filter*) |
| `Cmd+Shift++` / `Cmd+Shift+-` | `Alt+Shift++` / `Alt+Shift+-` | Raise / lower the parallel-port sampler input gain (only when a sampler is attached; also *Parallel Port > Sampler Gain*) |
| `Cmd+Shift+>` / `Cmd+Shift+<` | `Alt+Shift+>` / `Alt+Shift+<` | Raise / lower the host mouse sensitivity (also the launcher's Input tab) |
| `Cmd+F` | `Alt+F` | Toggle fullscreen on / off |
| `Cmd+Shift+F` | `Alt+Shift+F` | Show / hide the status bar |
| `Cmd+P` | `Alt+P` | Toggle the [performance overlay](#performance-overlay) (`[display] perf_overlay` sets the start-up value) |
| `Cmd+W` | `Alt+W` | Toggle Warp Speed (turbo) on / off; one press also ends every warp a control client, a GDB client, the guest, or a boot phase engaged |
| `Cmd+Shift+W` | `Alt+Shift+W` | Cycle the Warp Speed limit: 2x, 4x, 8x, 16x, Max |
| `Cmd+Z` | `Alt+Z` | Rewind the machine one step (needs `[emulation] rewind` or *Emulation Settings > Rewind*) |
| `Esc` | `Esc` | Close an open menu or overlay panel (in a tool window, that window); otherwise passed through to the Amiga |
| `Ctrl+Amiga+Amiga` | `Ctrl+Amiga+Amiga` | Keyboard reset (warm reboot) |

Host modifiers that are passed through to the emulated keyboard map onto
the Amiga keyboard: Alt becomes Amiga Alt, Cmd/Super becomes the left/right
Amiga keys, and left Ctrl becomes Amiga Ctrl, so `Ctrl+Amiga+Amiga` is
typed naturally. The Amiga keyboard has no right Ctrl, so host right Ctrl
also acts as the right Amiga key -- handy on PC/laptop keyboards that lack
a right Super/Win key.

All other keys are sent to the emulated machine through the real path: a
bit-timed keyboard-MCU model clocks each transition into CIA-A's serial
register over the emulated KCLK/KDAT lines, with the real handshake,
power-up stream, and recovery protocol -- so even software that talks to
the keyboard hardware directly behaves. `Ctrl+Amiga+Amiga` runs the
authentic reset protocol (reset warning, then KCLK held low), so the
reboot lands a fraction of a second after the chord, as on real hardware.

(keyboard-and-controller-navigation)=
## Keyboard and controller navigation

Everything the mouse can reach in the launcher, the menu, the status bar
and the overlay panels can be reached with the arrow keys or a
controller.

The arrows move the focus, which lights the control it is on. Return (or
the pad's fire button) works it; `Esc` (or the pad's second button) steps
back out -- out of an open setting, then the page, then the surface.
Left and right move along the row, up and down to the row above or below;
at the edge of a surface the focus stays where it is. Controls that
cannot be pressed are skipped.

| Control | Return / fire |
|---|---|
| Buttons, tabs, the close gadget | Presses it |
| Tick boxes and cover art | Ticks it |
| Text boxes | Opens it for typing |
| `< value >` settings | Opens it; left and right change the value, Return closes it |
| Volume slider | Opens it; left and right move it |

Lists -- the games, the favourites, the host's disks -- are walked with
up and down, which move the list's own selection and scroll it. Right
steps to the tick beside a row and then out of the list; left leaves it.

Stepping down off the foot of a surface reaches the status bar, and up
returns. Left out of a settings page returns to the category button that
opened it, and right opens that button's page. Walking off the bottom of
the menu closes it and leaves the focus on the menu button.

Clicking anything puts the focus away and leaves it where the pointer
pressed, so going back to the keyboard resumes from there. While the
focus is shown the pointer highlights nothing; moving the mouse puts it
away again.

A controller drives all of this with its d-pad, fire and second button.
Its [Menu button](#gamepad-calibration) is the way in from a running
machine, and the way out too: the menu's last row is **Quit**, and the
Menu button *held* quits -- unless the pad's calibration bound a
separate Quit control, which then owns the hold instead.

The debugger, frame analyzer and console windows are not navigated this
way, but `Esc` closes the focused one and the pad's second button closes
the top one.

## Status bar

The status bar (44 pixels below the display) holds, left to right (it can
be hidden entirely with `Cmd+Shift+F` / `Alt+Shift+F` or *Video Settings >
Status Bar*):

- **LED block.** PWR and FDD always; a green HDD activity LED on machines
  with a hard-disk controller (Gayle or A4000 IDE, or any SCSI adapter) or a
  host-folder filesystem mount (`[[filesys]]`), lit while either is accessed; a
  blue CD activity LED on CDTV/CD32 that lights while the drive reads data
  or plays CD audio (on a machine whose CD drive is a SCSI CD-ROM unit,
  the LED shows CD-DA playback; its data reads ride the HDD LED with the
  rest of the SCSI bus). Small digital counters show the current floppy and
  CD tracks, in green and blue respectively. A machine with only one kind of
  removable drive gets one full-size counter; a machine with both gets two
  vertically stacked counters, and a zero-floppy CDTV/CD32 does not show an
  empty FDD counter. When a built-in CD controller and an expansion CD-ROM are
  both fitted, the CD LED, counter, and media buttons stay assigned to the
  built-in drive. The PWR LED is lit whenever the machine is powered,
  driven by the guest's /LED line the way it drives the LED on an A500
  rev 6 or later board: full brightness while the line is engaged (Paula's
  analogue filter on), dimmed -- not extinguished -- once the software
  releases it.
  It follows the pin itself, so the **Audio Filter** menu override changes
  what you hear, never the LED.
- **Per-drive floppy controls.** Every connected drive gets a disk button
  (marked with the drive number) that opens a file dialog -- multi-select
  several images to queue a swap playlist for that drive -- plus a swap
  button that cycles to the next queued disk and an eject button. Swap and
  eject grey out when there is nothing to swap to or eject. With three or
  four drives the clusters stack two-up. A bay driving a physical floppy drive
  keeps its numbered disk button, so you can see the drive is there, but
  loading, swapping and ejecting do nothing for it -- the disk is in a real
  drive, and changing it is done by hand (see [](fluxbridge.md)).
- **CD controls** on machines with a CD drive (CDTV, CD32, or a SCSI
  CD-ROM unit): a CD button that loads (or swaps) a CD image
  (`.cue`/`.iso`/`.nrg`/`.chd`) with the proper media-change notification, and a
  CD eject button. These do not appear on machines without a CD drive.
- **Joystick toggle** (just left of the volume control): a gamepad or
  keyboard icon showing which source drives the joystick port. Click it to
  flip between gamepad-only and keyboard joystick emulation; see
  [](#controller-ports).
- **Keyboard toggle** (just left of the joystick toggle): a small keyboard
  icon, lit while the on-screen Amiga keyboard is up. Click it to show or
  hide it (see [](#on-screen-keyboard)).
- **Volume slider**: drag, or scroll the mouse wheel over it for 5% steps.
- **Hamburger menu button**: opens the pop-up menu (below).
- **Camera button**: saves a screenshot (same as `Cmd+S` on macOS or
  `Alt+S` on Linux/Windows).
- **Pause / power / reboot buttons.** Pause freezes emulation while staying
  powered; power cold-boots (reinitialising RAM with the Memory page's
  Power-on fill policy) or powers off back to the test screen; reboot is a
  warm reset.

(on-screen-keyboard)=
## On-screen keyboard

The keyboard button in the status bar, or *Input Settings > On-Screen
Keyboard*, draws an Amiga keyboard in a strip between the display and the
status bar. The window grows to make room for it: the picture keeps its
size, the canvas gets taller, and hiding the keyboard gives the height
back. A window you have resized yourself keeps its size, and the display
reflows into it as usual.

It is an **A600** -- the one Amiga keyboard with no numeric keypad, so the
whole machine fits the window's width at a usable cap size. Clicking a cap
sends that key's rawkey to the emulated keyboard MCU over the same
authentic serial protocol a host keystroke uses, and is recorded the same
way, so on-screen keys are captured by `--record-input` and replay from the
resulting `--script` file. The two keyboards are independent holders of the
same key: pressing a cap the host keyboard is already holding down changes
nothing for the machine, and the key comes up only when the last of the two
lets go.

The keyboard is the way to reach the keys a host keyboard has no
equivalent of -- Help, both Amiga keys, and the `#`/`~` key beside Return --
and the way to drive a session entirely with the mouse.

- **Qualifiers latch.** To enter keyboard chords with sequential clicks,
  Ctrl, both Shifts, both Alts and both Amiga keys stay down when clicked: click one and it
  is held for the next keystroke, then released with it. Click it twice in
  quick succession to lock it down until you click it again; click a locked
  one to let it go. Press and *hold* one instead, and it behaves like the
  real key, coming up when the button does. A latched qualifier is drawn
  with an orange ring, a locked one filled orange.
- **Ctrl+Amiga+Amiga** is typed by latching the three, as any other chord
  is. The moment the chord completes, the keyboard lets go of all three --
  the MCU's reset protocol is already running, and qualifiers still held
  would be reported held again through the power-up stream.
- **Caps Lock** carries a lamp in the corner of its cap, driven by the
  keyboard MCU itself rather than by the clicks, so it follows a
  save-state load or a guest that toggles the key in software.
- **UK/US legends.** The chip marked UK/US in the cursor notch swaps the
  five caps a US A600 prints differently (`2`, `3`, `'`, and the two keys a
  US machine ships blank). It changes what the caps say, not what they
  send: the guest's own keymap decides that.
- **The X chip** above it puts the keyboard away, the same as the status
  bar button. It sits in the slot farthest from the cursor keys, so a
  missed arrow cannot fold the keyboard away mid-game.

(performance-overlay)=
## Performance overlay

`Cmd+P` (macOS) / `Alt+P`, *Video Settings > Performance*, or
`[display] perf_overlay = true` shows a live emulation-performance readout
in the top-right corner of the display, one line per data point at the
menu's font size (it follows *Menu Size*), refreshed twice a second:

| Line | Meaning |
|---|---|
| `50.0 fps` | Emulated video frames retired per host second: 50 (PAL) or 60 (NTSC) when the machine holds real time, lower when the host cannot keep up, far higher under Warp Speed |
| `x1.00` | Speed factor: emulated seconds advanced per host second (the effective multiplier under Warp Speed) |
| `emu 3.2 ms` | Host milliseconds of emulation work per emulated frame, pacing sleeps excluded |
| `host 16%` | Share of host wall time spent emulating. In real-time mode this is the share of the frame budget (20.0 ms PAL, 16.7 ms NTSC) used per frame |
| `audio 148 ms` | Live audio output lead -- the cushion that absorbs host scheduling hiccups (steady state is about 150 ms) |
| `xrun 0` | Audio underrun frames per second: the audible symptom of the host falling behind |
| `slip 0` | Times the pacer reset its host-time target after exceeding the catch-up limit (for example, after a host stall), since the last guest reset |

Copperline never skips frames to keep pace: when the host cannot sustain
real time the machine slows down (fps and the speed factor fall below
nominal while `host` sits near 100%). After a long stall, the pacer resets
its host-time target and increments `slip`; guest execution continues from
the same point. Under Warp Speed
the presentation shows one frame per burst, but every emulated frame is
still computed.

Like the transient message overlay, the readout is painted into the
presentation only: screenshots, frame dumps, and recordings never include
it. While a video recording is running the block steps below the `REC`
badge. `COPPERLINE_PERF_OVERLAY=1|0` overrides the config for a single
run, and `--perf-overlay` shows it from the command line; the launcher's
*Perf overlay* row (*A/V & Emu*, *Display*) makes it stick. The same
counters are exported through the control protocol's `status` reply for
headless runs (see [](../debugger/control)).

## Guest debug overlay

Guest programs (such as those using the vscode-amiga-debug template) can render
custom debug shapes and text onto the screen using uaelib trap helpers
(`debug_rect`, `debug_filled_rect`, and `debug_text`; see [](run)). Overlay
commands use a virtual 768x576 coordinate space mapped across the active display
area, adapting automatically to autocrop and scaling settings until cleared with
`debug_clear`. The debug overlay is rendered beneath host UI elements (status
bar, OSD, performance overlay) and affects only live window presentation;
screenshots, frame dumps, and video recordings exclude it. Disabling
`[emulation] uaelib` removes the uaelib trap entirely.

## Drag and drop

Disk images can be dropped anywhere on the emulator window:

- **Floppy images** (ADF/ADZ/DMS/IPF/SCP, gzip or zip packed): with one
  connected drive the disk is inserted immediately. With several, a drive
  chooser opens over the display -- click a drive, press its number
  (`1`-`4`), or press `Esc` to cancel. Dropping several floppies at once
  queues them all as the target drive's swap playlist, exactly like a
  multi-selection in the disk dialog.
- **CD images** (`.cue`/`.iso`/`.nrg`/`.chd`) mount in the machine's CD drive
  (CDTV, CD32, or a SCSI CD-ROM unit), with the media-change notification.
- **WHDLoad packages** (`.lha`, `.zip`, or a bare `.slave`) reboot the
  machine straight into the game through the [WHDLoad booter](whdload.md),
  keeping any explicit machine choices; dropped on the configuration
  screen they fill the **WHDLoad** page's game field instead.
- **Hard disk images and Kickstart ROMs** are not accepted as drops. Configure
  hard disks in the machine-configuration screen. To replace a running
  machine's ROM and cold-reset it, use **Load Kickstart ROM...** from the menu.

The chooser opens after the drop rather than offering per-drive drop
targets because the windowing layer reports file drops without a cursor
position. For the same reason drops are unavailable under native Wayland
(X11/XWayland works).

## Menu, tool windows, and overlay panels

```{figure} ../images/ui-preview-menu-open.png
:alt: The pop-up menu with a category open
:width: 75%

The menu, with a category open beside it.
```

The hamburger button at the right of the status bar opens the menu, as does
`Cmd+E` / `Alt+E`. It grows upward from the status bar and dims the picture
behind it, so what it covers stays readable without competing for attention.
The dimming is a window effect: it never appears in a screenshot, a frame
dump, or a recording.

The top level holds the tools, then a category per area of the machine, then
the ROM, the shortcut reference and **About...** last. A category is marked
`>` and opens a list of its own beside it; a setting with more than two
values opens a further list of those values with the one in force ticked, and
a setting that is simply on or off is ticked in place. Categories with
nothing to offer -- the serial and parallel ports on a machine with nothing
on them -- are not shown at all.

Point at a category and it opens; point at one of its rows and that row is
where the keyboard is. The cursor keys walk the same path -- up and down
within a list, right into a category, left back out -- and Return picks the
row under the cursor. `Esc` steps back one level, and closes the menu from
the top.

Picking a row that opens something else -- a window, a panel, a file dialog
-- closes the menu behind it. Changing a setting leaves the menu up with the
new value marked, so a run of changes costs one open rather than one each.
Opening the menu also releases a captured mouse.

**Menu Size** under *Video Settings* draws the whole menu at 1x or 2x. The
start-up size is `[display] menu_scale`, `--menu-scale`, or *Menu size* on
the launcher's A/V & Emu page (Display category).

Tool windows are separate native windows so the emulated display remains visible;
the debugger and frame analyzer can be open at the same time. They take their
keys and clicks through their own windows, and the main window keeps driving
the Amiga while they are open -- resume the machine from the debugger and you
can play on while watching it. Overlay panels are drawn over the display and
*are* modal: while one is open, key presses and display clicks stay in the UI
instead of reaching the Amiga. `Esc` in a tool window closes that window;
`Esc` in the main window closes the menu or overlay panel, and otherwise
belongs to the Amiga.

### Tools

- **Machine Configuration...**: opens the configuration screen
  ([below](#machine-configuration-screen)) to reconfigure the machine and
  relaunch it. The same screen opens automatically on a no-machine start.
- **Frame Analyzer...**: pauses the machine and opens a separate diagnostic
  window with three tabs: which chip-bus owner had each Agnus colour clock
  across the captured frame, including overscan and blanking, with a CPU
  wait view that attributes every clock the CPU was denied to the DMA
  channel that held it and names the stalled instructions; a memory
  heat map of what last touched each part of the address space; and the
  debug resources the guest registered through the uaelib trap, with
  decoded previews; see [](../debugger/window.md#frame-analyzer-pane).
- **Debugger...** (also `Cmd+B` / `Alt+B`): pauses the machine and opens the
  tabbed debugger in a tool window; see [](../debugger/window).
- **Console...** (also `Cmd+K` / `Alt+K`): a GDB-flavoured debugger
  command line in its own tool window; see [](../debugger/console).
- **Freeze (HRTMon)** (also `Cmd+Shift+B` / `Alt+Shift+B`): presses the
  freezer cartridge's button, so the machine runs on into the HRTMon
  monitor on its own screen; the monitor's `x` command returns to the
  program. Greyed out unless a cartridge is fitted (`[cartridge] model`,
  or the configuration screen's *Freezer cartridge* row); see
  [Configuration](configuration.md#freezer-cartridge).

### Audio Settings

- **Audio Output** (also `Cmd+Shift+A` / `Alt+Shift+A`): the system default,
  any host output device, or **Disabled** (sound off entirely, equivalent to
  `--noaudio`). The device switches live without a restart.
- **Audio Filter** (also `Cmd+A` / `Alt+A`): Paula's analogue low-pass
  filter -- **Auto** (guest-driven, the default), **Enabled**, or
  **Disabled**. The forced settings apply regardless of what the software
  asks; auto restores hardware behaviour. The PWR LED keeps following the
  guest's /LED line whatever is forced here.

### Video Settings

- **Menu Size**: 1x or 2x, described above.
- **Pixel Aspect**: the 4:3 CRT pixel aspect (the default; PAL lo-res pixels
  slightly wider than tall, as a real TV shows them) or square pixels (a
  320x256 screen is an exact 640x512, handy for pixel-exact comparison with
  square-pixel emulators). The window and its backing texture resize with the
  mode. The start-up mode comes from `[display] pixel_aspect`
  (see [Configuration](configuration.md)).
- **Scaling**: How the display canvas is mapped to the window:
  - **Smooth** (default): Fits the display canvas to the window while preserving
    aspect ratio, using linear interpolation.
  - **Integer**: Scales using whole-number multiples of the native canvas,
    centered with black borders and point-sampled. Under the default TV aspect,
    horizontal and vertical axes scale using independent integer multiples to
    preserve the 4:3 aspect ratio (e.g. 4:5 pixel aspect for NTSC on 1080p).
    In square pixel mode, scaling is uniform. Integer scaling also applies to
    RTG modes at multiples of native resolution, falling back to smooth scaling
    if the window is smaller than 1:1. Toggling integer scaling in TV mode
    resizes the window to the native canvas. Configured via `[display] scaling`
    (see [Configuration](configuration.md)).
- **Autocrop**: Automatically crops the presentation to the active raster lines
  programmed by the Amiga hardware, allowing games with smaller viewports to
  fill more of wide screens. When combined with integer scaling, the integer
  scale multiplier is recalculated against the cropped area. The status bar and
  overlay panels remain docked at the bottom of the window; opening a menu or
  panel temporarily expands the display to the full area so controls remain
  accessible. Presentation-only (captures always use full aperture), supports
  CRT shaders, works with programmable multisync modes, and is suspended when
  using monitor bezels or RTG modes. Configured via `[display] autocrop` (see
  [Configuration](configuration.md)).
- **Screen Centring**: nudge where the TV picture sits on the glass, the
  H-CENTER/V-CENTER controls a real monitor carried on its front.
  **Picture Left/Right** step one lo-res pixel (up to 16 each way),
  **Picture Up/Down** one scan line (up to 8), **Reset** recentres; the
  category row shows the current nudge. Stepping right brings the
  captured left overscan into view -- artwork that leans off the default
  view, like the CD32 boot logo's leading serif -- while glass nudged
  past the captured raster shows black, as past the raster's edge on a
  real tube. A TV-aperture control, so it is greyed under
  `overscan = "full"`, which already shows everything. Unlike the shader
  and tint it is picture geometry, not a window effect: screenshots and
  frame dumps follow it. Session-only; the start-up values are
  `[display] tv_h_centre` / `tv_v_centre`
  (see [Configuration](configuration.md)).
- **CRT Shader**: the GPU tube-emulation pass over the picture --
  **Disabled**, **Scanlines** (the line structure of a 15 kHz set), **Mask**
  (an RGB phosphor shadow mask), **CRT (1084)** (both, plus a bowed tube face
  and a corner vignette), and **Custom** when the config named a shader of
  your own, which is re-read from disk each time it is chosen. The change is
  session-only: the start-up preset, the strength knob, and how to write a
  custom shader are `[display] shader` and `shader_strength` (see
  [Configuration](configuration.md)). The pass is a window effect only --
  screenshots, frame dumps and recordings are never shader-processed -- and
  it steps aside for the frames it cannot sensibly draw: while the menu or
  any panel is open, under RTG, and in programmable multisync scan modes.
- **Shader Strength**: how strongly the shader pass is applied, stepped
  10% at a time with **Stronger** / **Softer** (the category row shows the
  figure); greyed while the shader is off. The start-up strength is
  `[display] shader_strength` (see [Configuration](configuration.md)).
- **Screen Tint**: the monochrome-monitor tint over the picture --
  **Colour** (full colour), **Black & white**, **Green** and **Amber** (the
  two classic monochrome phosphors), and **Sepia** -- the same looks as the
  web frontend's *Screen* selector. Session-only, like the shader; the
  start-up tint is `[display] tint` (see [Configuration](configuration.md)).
  A window effect only: captures stay untinted, the menu and the status bar
  keep their colours, and RTG scanout is never tinted.
- **Fullscreen** (also `Cmd+F` / `Alt+F`): borderless fullscreen on the
  window's current monitor. The picture keeps its aspect with black letterbox
  bars as needed, exactly as when resizing the window.
- **Status Bar** (also `Cmd+Shift+F` / `Alt+Shift+F`): show or hide the
  status bar. Handy alongside fullscreen for a clean, chrome-free picture.
- **Monitor Bezel**: which monitor front the picture sits inside instead
  of filling the window -- **Disabled**, **1084** (a two-tone cabinet with
  the tube sunk into its moulding, and the model badge, the Copperline name
  and the power lamp along the bottom), or **Classic** (the plainer rounded
  frame Copperline drew first). The picture gets a little smaller to make
  room for either, and the tube shows a little more of it: the whole
  captured raster fills the glass edge to edge, border colour and all,
  so the opening's rounded corners crop into the overscan border the way
  a real tube's do, not into the picture.
  `Cmd+M` / `Alt+M` turns the chosen front off and back
  on; it never changes which. Session-only; the start-up value is
  `[display] bezel` (see [Configuration](configuration.md)). A window
  effect only, like the shader: screenshots, frame dumps and recordings
  never include it. A folder of PNG stickers can be drawn onto either
  front -- die-cut decals on the plastic, riding the same toggle -- via
  `[display] bezel_stickers` (no menu item; see
  [Configuration](configuration.md)).
- **Performance** (also `Cmd+P` / `Alt+P`): show or hide the
  [performance overlay](#performance-overlay). Session-only; the start-up
  value is `[display] perf_overlay` (see
  [Configuration](configuration.md)).

### Input Settings

- **Port 1 Device / Port 2 Device**: hot-plug the controller in a game
  port -- Mouse, Joystick, CD32 pad, Analogue, or None; see
  [](#controller-ports).
- **Joystick Input** (also `Cmd+J` / `Alt+J`, or the status-bar icon):
  Gamepad-only or Keyboard joystick emulation.
- **Autofire**: the autofire rate (off, 3, 5, 8, 12, 16 Hz), which pulses a
  *held* fire button on live gamepad and keyboard input. Scripted input is
  never gated; see the `[input]` section of
  [Configuration](configuration.md).
- **On-Screen Keyboard** (also the status-bar keyboard icon): draws an
  Amiga keyboard under the display; see [](#on-screen-keyboard).
- **Calibrate Gamepad...**: the guided calibration flow, described below.
- **Input Mapping...**: edits which host keys drive the controller controls,
  for both keyboard mappings; see [](#input-mapping).

### Serial Port and Parallel Port

Shown only when something is on the port.

- **MIDI In / MIDI Out** (serial port in MIDI mode): Paula's serial bridge
  onto the host's MIDI sources and destinations; see the `[serial]` section
  of [Configuration](configuration.md). **MT-32** is offered here too,
  and with it playing, an **MT-32** submenu carries its front panel and
  display style; see [The MT-32](mt32.md).
- **Sampler Input / Sampler Gain** (parallel-port sampler attached): the
  sampler's host capture device, and its input gain, which the *Increase* and
  *Decrease* rows step (also `Cmd/Alt+Shift +/-`). Both change live. See the
  `[parallel]` section of [Configuration](configuration.md).

### Emulation Settings

- **Floppy Speed**: the emulated drive speed -- 100% (real speed), 200%,
  400%, 800%, or turbo (disk DMA transfers complete almost instantly).
  Changes apply to the live machine immediately. The start-up value comes
  from `[floppy] speed`; see [Configuration](configuration.md) for what each
  level preserves and the compatibility trade-off.
- **Rewind** (the hotkey is `Cmd+Z` / `Alt+Z`): records rewind history. While
  it is on, the emulator keeps a ring of whole-machine snapshots and the
  hotkey steps the machine back through them -- the whole machine, not just
  the picture: CPU, chips, RAM and all. One press goes back one capture
  interval (half a second of emulated time by default). Turning it off
  releases the snapshots, which is where the memory goes; the budget and
  interval are `[emulation] rewind_budget_mb` and `rewind_interval_frames`,
  and `rewind = true` starts recording at launch (see
  [Configuration](configuration.md)). It shares its substrate with the
  debugger's reverse controls, so the same determinism caveats apply -- see
  [](../debugger/reverse).
- **Run Ahead**: input-latency reduction for play. Each display refresh
  commits one frame, runs a few silent future frames, presents the last future
  image, and rewinds to the committed boundary. Level **1 frame** is the best
  starting point; higher levels need proportionally more host CPU (watch the
  performance overlay) and skip more intermediate animation. When a live
  device, writable medium, debugger, capture, or other host coupling makes
  speculation unsafe, selecting a level keeps it configured but the OSD says
  why it is inactive. The start-up value is
  `[emulation] run_ahead_frames` or `--run-ahead`; the full compatibility list
  is in [Configuration](configuration.md).

### Warp Settings

- **Warp Speed** (also `Cmd+W` / `Alt+W`): runs the emulator unpaced for
  fast-forward. Toggling back re-anchors real-time pacing cleanly. A
  control-protocol client (`warp.set`), a GDB client (`monitor warp`), or the
  guest program (`warpmode()` through the uaelib trap, see
  [Direct launching](run.md)) can engage warp too; each holds it
  independently and releases only its own, such a warp mutes live audio, the
  OSD names who asked, and one press of the shortcut ends them all.
- **Warp Limit** (also `Cmd+Shift+W` / `Alt+Shift+W`): how fast warp runs.
  Because the window presents with vsync, emulating one frame per presented
  frame would cap warp at the host monitor's refresh rate (about 1.2x for
  50 Hz PAL on a 60 Hz display). The limit sets an output frame skip -- 2x,
  4x, 8x, 16x, or **Max** -- so warp retires that many emulated frames per
  presented frame, making the effective speed roughly the limit times the
  refresh rate (host CPU permitting). `Max` runs flat out and still presents
  at vsync. The default is set by `[emulation] warp_speed` (see
  [Configuration](configuration.md)).

### Recording

- **Record Video** (also `Cmd+R` / `Alt+R`): starts a video-with-audio
  recording; the same row (or shortcut again) stops it. See below.
- **Record Input** (also `Cmd+Shift+R` / `Alt+Shift+R`): records every
  input event that reaches the emulated machine; stopping writes a script
  file that `--script` replays deterministically. See below.

### Save State

- **Quick Save** and **Quick Load** each list all ten numbered slots, naming
  when the slot was written or showing it as empty, so an overwrite is
  visible before it is chosen. No dialog, and no file to name. The hotkeys
  reach the same slots -- `Cmd/Alt+<digit>` saves and
  `Cmd/Alt+Shift+<digit>` loads, with `0` as the tenth. See below.
- **Save State...** (also `Cmd+Shift+S` / `Alt+Shift+S`) and
  **Load State...** (also `Cmd+Shift+L` / `Alt+Shift+L`): snapshot the whole
  emulated machine to a file of your choosing, or restore one and continue
  from exactly that point. See below.

(and-last)=
### Application controls

- **Load Kickstart ROM...**: fit a different boot ROM. Pick a 512 KiB
  Kickstart or a 256 KiB Kickstart 1.x image, then optionally a second file for the extended ROM (512 KiB at
  $E00000 or 256 KiB at $F00000; Cancel to skip and remove any fitted
  extended ROM). The machine then cold-resets, as if the chip had been
  swapped and the power cycled. The OSD names the image it fitted -- the
  file name plus the Kickstart it was identified as, e.g.
  `ROM: kick40068.A1200 (Kickstart 3.1 (40.68) A1200)`.
- **Keyboard Shortcuts...**: the shortcut reference.
- **About...**: app version plus a summary of the emulated machine -- its
  `ROM:` line names the boot ROM file and, for a released image, which
  Kickstart it is (identified by checksum, see [Configuration](configuration.md)) -- and
  credits including Copperline's contributors and Patreon sponsors (see
  `CREDITS.md`). Builds
  made from an untagged git commit append the short commit ID to the version
  shown in the window title and About panel.
- **Quit** (also `Cmd+Q` / `Alt+Q`): exits Copperline. It is the last row
  so that a [controller or keyboard walking the
  menu](#keyboard-and-controller-navigation) finds it at the foot, with
  nothing below it to pick by mistake; there is no confirmation, as with
  the shortcut.

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
  A600, A1200, A3000, A4000, CDTV, or CD32. With no profile chosen the A500 is
  highlighted,
  since that is the machine the defaults describe. Selecting a machine applies
  that profile's defaults (chipset, CPU, RAM, gate array, RTC) to every tab;
  settings that no longer apply (an IDE image on a machine with no IDE port, a
  CD image
  on a machine with no CD drive) are dropped so they cannot block a launch.
- **Category tabs** (left sidebar). *System* (chipset and Agnus/Denise
  overrides, video standard, RTC, identify board, RTG card, freezer
  cartridge), *CPU* (model,
  FPU, clock, caches, and the experimental not-cycle-exact JIT mode --
  see `[cpu] jit` in [Configuration](configuration.md)),
  *Memory* (cold power-on fill -- zero, deterministic random, or a typed fixed
  16-bit word -- plus chip/fast/slow/motherboard/accelerator/Zorro III RAM),
  *ROM*
  (Kickstart, extended ROM, and the CD32-only **FMV module ROM**; the
  Kickstart row carries **Name**, **Version** and
  **Revision** lines naming what the chosen image is, identified by
  checksum rather than by file name -- blank for an image Copperline does
  not know, and read from the image itself for the bundled AROS. The FMV row
  shows the bundled open ROM by default on CD32, accepts another 256 KiB module
  ROM, and has a **Remove** / **Default** action for leaving the cartridge slot
  empty or restoring the bundled module. It is greyed on non-CD32 profiles;
  see [Configuration](configuration.md),
  *Floppy* (drive count from zero to four -- CDTV/CD32 default to zero -- and
  speed, then each wired drive as a
  greyed **DFn:** heading with its indented disk image and write-protect;
  drives that are not enabled are hidden rather than greyed. A drive holding
  an image stays wired until that image is cleared, so reducing the count
  cannot hide media that will still be present when the machine starts. Each
  drive also carries a **Physical drive** tick box that hands the bay to a physical
  floppy drive: its media row then names the interface -- or `None` with nothing
  plugged in -- and a **Configure** button opens that drive's own page,
  headed with the built-in FluxBridge library and its version, for the
  serial port, drive select, density, read mode and replay speed, greying
  whatever the chosen interface does not honour. See [](fluxbridge.md)),
  *Storage* (IDE master/slave -- either can be a CD image instead of a hard
  disk, attaching an ATAPI CD-ROM drive there -- and the SCSI controller --
  A2091, A4091, or the A3000's onboard SCSI. Its boot ROM and unit rows, a
  unit likewise a CD image attaching a SCSI CD-ROM drive, appear once a
  controller is chosen and are hidden with none, the same way disabled
  floppy drives are; a block of buttons at
  the top links to seven sub-pages: **CD** (image, insert delay,
  CD32 NVRAM); **Host Folder**, for host directories
  served live as AmigaDOS volumes (up to four mounts, each with a boot
  priority and a read-write/read-only **Access** field -- the config file
  itself takes up to eight `[[filesys]]` mounts, of which the launcher edits
  the first four); **Host Disk**, for a real disk of this computer's (see
  [](host-disks.md)); **Lide**, the built-in `[lide]` Zorro II IDE board --
  personality (RIPPLE/RIDE/AT-Bus 2008), boot ROM(s), and up to four drives
  (two on RIDE/AT-Bus 2008, any of which can likewise be a CD image);
  **Copperline HD**, the emulator's own virtual hardfile controller
  (`copperhf.device`, configured as `[copperhf]` -- see
  [](configuration.md)) -- up to seven units, always
  shown with no controller or personality to pick first (there is no real
  hardware behind it), and hard disks only: unlike IDE, SCSI, and Lide a
  Copperline HD unit cannot be a CD image. A unit configured here attaches at
  boot, the same as any other drive; swapping one's media while the machine
  runs is not a screen or menu action here but a
  [control-protocol](../debugger/control.md) call
  (`copperhf.attach`/`copperhf.eject`), which the window surfaces with an
  on-screen notice the way any other host-initiated media change is;
  **Boot Priority**, which sets each drive's
  synthesized-RDB boot priority (see below); and **Create Image...**, which
  makes new ADF and HDF images (see below). Each sub-page has a **< Back**
  button in that block that returns to Storage),
  *Input* (the controller device in each game port and the joystick input
  source),
  *I/O Ports* (the serial, parallel, networking, and audio boards, each
  on its own page -- **Serial Port**, **Parallel Port**, **Networking**,
  **Audio** -- switched between on the top nav row, with
  each port's options indented
  beneath its heading: serial mode and MIDI endpoints, with the emulated
  MT-32's ROM
  images, front panel and display style when it is the chosen output (see
  [The MT-32](mt32.md)), and, for TCP and modem modes, the connection
  address -- **Connect** (`tcp-connect`) for the remote target, or **Listen**
  (`tcp`, `modem`) for the local bind address -- configured via host and
  port fields. Leaving a field blank uses the default (host `127.0.0.1` for
  Listen, port `1234`; Connect requires a host/IP). Valid ports are 1-65535;
  on macOS and Linux, binding a Listen port below 1024 requires root
  privileges (outbound connections have no port restrictions); the
  parallel device -- None, Printer, or Sampler -- with, for the printer, its
  capture output file, or for the sampler, its host audio input and input gain;
  and the A2065 Ethernet and HostSocket bsdsocket.library boards, each --
  None, Isolated, Loopback, NAT, or Bridged; Bridged adds a host-adapter
  row. NAT and Bridged show a warning because host-clocked traffic makes
  input recordings and save-state replays non-reproducible while it flows;
  and the Toccata and MHI sound boards, each a plain fit/don't-fit toggle
  with no other options -- host-side audio capture and backend settings
  such as `--audio-wav`, `--audio-stems`, and device selection stay
  command-line/config-file only and have no row here),
  *Zorro* (extra autoconfig boards by metadata file, with a config panel for a
  WASM plugin board's declared options),
  *WHDLoad* (your game collection, and the settings games boot with -- see
  [](whdload.md)),
  and *A/V & Emu*, split by a row of category buttons at the top into
  **Audio** (output device, channel mode, stereo separation, filter, floppy
  sounds and volume), **Video** -- the emulated picture (monitor bezel style,
  overscan, pixel aspect, scaling, deinterlace, screen tint, phosphor, CRT
  shader and shader strength), **Display** -- the host window (start
  fullscreen, status bar, perf overlay, menu size),
  **Emulation**
  (power-on, run on startup -- `[emulation] auto_launch`, the launcher
  running an opened configuration at once -- realtime priority, pacing,
  warp speed),
  and **Paths** -- opening on Audio, and switched freely between the five.
  The Paths page edits the `[paths]` section of the configuration (see
  [](configuration.md)): the base folder on top, then one row per folder.
  A row reads `(default)` until a folder is chosen for it; **Browse** picks
  one and a **Reset** button then appears to put the row back to inheriting.
  The base folder always shows its full path, and swaps Browse for Reset
  once set. Changes take effect immediately -- a screenshot taken after
  moving the row lands where the row now says -- and are written to the
  configuration by **Save As** or **Save default** like everything else.
- **Settings rows** (right pane). `[<]`/`[>]` step through a value, On/Off
  buttons flip a toggle, and the **Browse** and **Clear** buttons set or remove
  a file path through a native file dialog. On the *Storage* tab (IDE master/
  slave, a SCSI unit, a lide drive, or a Copperline HD unit), **Browse** lets you
  pick a directory as well as a file -- on macOS -- since any of those slots
  can be a host directory mounted as an in-memory FFS volume instead of a raw
  image; on other platforms the dialog is file-only there too, matching the
  rest of the launcher, and a directory target still has to be set some other
  way (e.g. editing the config file directly). Once an IDE, SCSI, lide, or
  Copperline HD drive has an image a small editable box appears next to **Browse**:
  click it and type to set the volume name for a directory mount (left blank, a
  directory mount inherits the host directory's name; the box has no effect on a
  raw HDF). Once that image is a host **directory** specifically, an **FFS/OFS**
  button appears just left of the volume-name box: click it to flip the
  in-memory volume's filesystem (FFS by default; OFS is the one every
  Kickstart from 1.2 onward can read with no guest-side setup).
  A setting that does not apply to the chosen machine is greyed and
  shows why in place of its control -- "needs 32-bit CPU" for Zorro III RAM
  and the RTG card, "needs 68020+" for the FPU, "needs A600/A1200/A4000 or Lide" for
  IDE.
- **Boot Priority sub-page** (from *Storage*). One row per hard-disk drive,
  under **Drive** / **Priority** / **Status** columns, setting the `de_BootPri`
  written into the partition Copperline synthesizes in front of a bare hardfile
  (it has no effect on an image carrying its own RDB). In the **Priority**
  column `[<]`/`[>]` nudge the value by one and the value box is also a text
  field -- click it and type any priority (-128..127), then Enter. The Status
  column's **Bootable** box is ticked by default; clearing it greys that row's
  priority and writes the -128 "disabled" sentinel, so the volume mounts but
  never boots.
  Priority arrows step each attached drive, including all seven Copperline HD
  units; they leave the remembered priority unchanged while Bootable is cleared.
  Drives without media or configured as CD-ROMs are disabled ("No drive" /
  "CD-ROM"). SCSI, Lide, and Copperline HD units appear only when media is
  attached, ensuring the list reflects valid boot sources. Drives are
  listed in order: IDE bays, SCSI units, Lide drives, and Copperline HD
  units. If the list exceeds one page, use **Next Page >** to walk forward
  through as many pages as the drives need; every page's **< Back**
  returns to the first. Newly added drives are assigned cascading default
  priorities (0 for the first hard drive, followed by -35, -40, -45) to
  avoid boot conflicts. A drive already carrying a priority in the config
  keeps it, and one that just names a device with no `bootpri` stays at 0. See
  [](configuration.md) for how the priority ranks against Kickstart's DF0: boot
  node at 5.
- **Action bar** (bottom). **Load...** reads a `.toml` config through a file
  dialog. **Save...** opens a small dialog of three buttons, each described
  in the dialog as the pointer moves over it: **Save As** writes a `.toml`
  through a file dialog (a minimal file, only the settings that differ from
  the chosen profile's defaults, so it reads like the example configs);
  **Save default** saves the running configuration as the one Copperline
  starts with: this screen opens showing it, and a run with no `--config`
  and no `./copperline.toml` boots it (see [](configuration.md));
  **Reset default** deletes that saved default after
  an "Are you sure?", returning Copperline to factory settings. The close
  gadget, or a click anywhere else, puts the dialog away.
  **Defaults** resets the screen to the selected profile. **Run** validates
  the configuration and boots it; if anything is wrong -- an unusable RAM
  size, a missing disk image, a ROM file that cannot be read, an option the
  chosen machine cannot use -- the reason is
  shown on the status line and you stay on the screen to fix it.

```{figure} ../images/ui-preview-launcher-boot-priority.png
:alt: The Boot Priority sub-page of the Storage tab
:width: 75%

The Boot Priority sub-page: an A1200 whose IDE master boots at priority 0, its
slave with the Bootable box cleared (showing the -128 that stores), and one
SCSI unit of a fitted A2091 carrying a disk of its own. A greyed **Info:**
label heads a note on the valid priority range.
```

Saved files use the same schema as a hand-written `copperline.toml`
(see [](configuration.md)), so the screen and the config file are
interchangeable: configure a machine and save it, or load an existing config to
tweak it. **Run** builds the machine in place, so the configuration screen and a
direct `--config` launch produce an identical machine.

### Create Image

*Storage -> Create Image...* makes new, empty disk images: **Floppy Disk**
writes an ADF, **Hard Disk** writes an HDF. These pages write a file and
nothing else; none of their settings belongs to the machine, so none of it
is saved to a configuration file.

**Save...** opens a file dialog, then writes the image. The status line
reports progress and the finished size; the write runs in the background,
so the window stays responsive while a large image is written.

#### Floppy Disk

| Option | Effect |
|---|---|
| **Density** | `DD (880K)` or `HD (1.76M)`. Sets the image size: 901,120 or 1,802,240 bytes. |
| **Container** | `Standard ADF` writes the sectors in order. `Extended ADF` wraps them in the `UAE-1ADF` container, which stores one record per track. |
| **Filesystem** | `Unformatted` leaves the image blank for the Amiga to format. `OFS` and `FFS` write a boot block, root block and bitmap. |
| **DOSType** | The DOS type's options: `International` case folding, `Dir cache`, `Long names`. See **DOSType** below. |
| **Volume name** | The name the volume mounts under. |
| **Bootable** | Writes the boot code that loads `dos.library`, so the disk boots rather than only mounting. |

#### Hard Disk

| Option | Effect |
|---|---|
| **Size** | A whole number up to 9999, in the unit beside it. Click `MB`/`GB` to change the unit. The image is rounded up to the next whole cylinder. |
| **Geometry** | `Auto` derives cylinders/surfaces/sectors from the size. `Custom` sets them by hand, and adds a **Configure** button that opens the geometry editor. |
| **Partitioning** | `RDB` writes a Rigid Disk Block and one partition filling the drive. `None` writes no partition table. |
| **Filesystem** | As for a floppy. With `RDB` the type is recorded in the partition entry; with `None` the volume starts at block 0. |
| **DOSType** | As for a floppy. |
| **Device name** | The device the partition mounts as, e.g. `DH0`. RDB only. |
| **Volume name** | The name the volume mounts under. |
| **Bootable** | Sets the partition's bootable flag. RDB only. |
| **Boot priority** | The partition's `de_BootPri`, -128 to 127. Kickstart enters DF0: at 5, so 6 boots the hard disk ahead of a floppy. Applies while Bootable is ticked. |
| **Read only** | Marks the finished file read-only on this computer. |
| **Sparse image** | On by default: the file is created at full length with only its structure written, and the host fills the rest in as it is used. Clear it to write the whole file now, which takes as long as writing that many bytes takes. |

An image larger than 2048 GB can be made only with `Partitioning: None`
and `Filesystem: Unformatted`: every block number an RDB or an AmigaDOS
volume uses is a 32-bit field.

With **Sparse image** cleared, the volume the file is being written to is
checked for room first, and the write is refused if there is not enough.

#### Geometry editor

Reached from **Configure** on the Hard Disk page once **Geometry** is set
to `Custom`. The geometry set here decides the image's size; the Size box
seeds it.

| Option | Effect |
|---|---|
| **Cylinders**, **Surfaces**, **Sectors per track** | The drive's stated geometry. Surfaces x sectors is one cylinder; the RDB states the partition in cylinders, so these set the granularity a partition can start and end on. |
| **Reserved blocks** | Blocks at the front of the partition the filesystem never allocates. Two -- the length of the boot block -- unless there is a reason to say otherwise. |
| **Drive**, **Type**, **Revision** | What the drive answers when asked what it is. HDToolBox shows the first two as its *Drive* and *Type* columns. Each box holds as many characters as its RDB field: 8, 16 and 4. |

**Apply** returns to the Hard Disk page. **Auto** puts every figure back to
what the size implies, and the identity back to `Amiga` / the size /
Copperline's version.

```{figure} ../images/ui-preview-launcher-new-geometry.png
:alt: The geometry editor of the Create Image sub-page
:width: 75%

The geometry editor, with the drive identity below the figures and a note
of the size they come to.
```

#### DOSType

The **Filesystem** row picks OFS or FFS; the **DOSType** row picks the
options that filesystem carries. Between them they name one of the eight
AmigaDOS types:

| Ticked | Type |
|---|---|
| -- | `DOS0` (OFS), `DOS1` (FFS) |
| International | `DOS2`, `DOS3` |
| International + Dir cache | `DOS4`, `DOS5` |
| International + Long names | `DOS6`, `DOS7` |

`Dir cache` and `Long names` are two values of one field, so ticking one
greys the other. Both are international, so `International` shows ticked
and greyed alongside either.

`Dir cache` needs Kickstart 3.0, `International` needs 2.0, and `Long
names` needs a filesystem no Kickstart provides.

## Recording video

`Cmd+R` on macOS or `Alt+R` on Linux/Windows (or the menu's "Record Video")
starts capturing the emulated display and sound to
`copperline-video-<YYYYMMDDHHmmSS>.avi` in the [recordings folder](#where-files-go); pressing it again
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
machine -- key presses with their hold times (typed on the host keyboard
or clicked on the [on-screen one](#on-screen-keyboard), which reaches the
machine by the same path), mouse buttons and motion, joystick / CD32-pad
controls, analogue pot positions, and floppy inserts,
on whichever port carries each device -- each stamped
with its emulated time. Pressing it again stops the recording and writes
`copperline-input-<YYYYMMDDHHmmSS>.clscript` in the [recordings folder](#where-files-go): a plain
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
`copperline-state-<YYYYMMDDHHmmSS>.clstate` in the [states folder](#where-files-go): CPU,
chip/slow/fast RAM, ROM, the full chipset and CIA state, floppy images
(including unsaved in-memory changes), expansion boards, and CD/NVRAM
state. `Cmd+Shift+L` / `Alt+Shift+L` (or "Load State...") restores one; the
machine continues from exactly the saved point, byte-for-byte -- the core
is deterministic, so a resumed run is indistinguishable from one that was
never interrupted.

States are taken at emulated-frame boundaries and are versioned per
subsystem: the file is a set of tagged chunks (CPU, memory, Paula, Agnus,
floppy, expansion boards, ...) that name their fields, so a state keeps
loading across releases that add or drop state fields, and a release that
changes what a subsystem's state means upgrades that chunk on load. A file
a build cannot read is refused with a message naming the chunk and
versions rather than producing a corrupt machine. A build still needs
support for every expansion board present in the snapshot. States written
by Copperline 0.19 and earlier used a flat layout with a single version
and must be recreated.

Saving over an existing file replaces it only after the new snapshot has
been completely written and flushed. A failed save leaves the previous
file intact.

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
- CD images are likewise reopened by path; keep the cue sheet and its
  files (or the CHD) where they were.

(quick-save-slots)=
### Quick-save slots

Naming a file is the wrong interaction for the "before this jump" save you
take twenty times an hour, so there are ten numbered slots as well.
`Cmd/Alt+<digit>` saves to that slot and `Cmd/Alt+Shift+<digit>` loads it,
with `0` as the tenth. The menu's **Quick Save** and **Quick Load** lists
reach the same ten, each row naming when that slot was written or showing it
as empty.

A quick save overwrites its slot without asking -- that is the point of it --
and loading a slot that has never been written reports "Slot N is empty"
rather than failing. Slots are ordinary `.clstate` files, identical in format
to a named save, kept in the states folder below. And because they are per
user and not per machine, a slot may hold a state from a different Amiga
than the one running. That is safe: as above, the state carries its own
machine and the load reconfigures to match it and says so.

The headless flags `--save-state-after SECS PATH` and `--load-state PATH`
script the same feature for [debugging workflows](headless.md): snapshot a
long-running program just before the scene under investigation once, then
iterate from the state in seconds instead of re-emulating minutes. The
file format and what exactly is (and is not) captured are specified in
[the internals chapter](../internals/savestate.md).

(where-files-go)=
### Where files go

Everything Copperline produces is kept per user rather than per working
directory, so it is reachable however Copperline was launched. The host-data
directory is:

| Host | Location |
|---|---|
| Linux/BSD | `~/Documents/Copperline/` |
| macOS | `~/Documents/Copperline/` |
| Windows | `%USERPROFILE%\Documents\Copperline\` |

with a folder inside it for each kind of file: `screenshots/`, `states/`
(named saves and the quick-save slots), `recordings/` (video captures and
recorded input scripts), `nvram/` (battery-backed clock RAM and CD32 game
saves), `traces/` (debugger traces and waveform captures), and `configs/`
(configurations saved from the configuration screen). Each folder is created
on first use, and each can be moved with the `[paths]` section of the
configuration or the configuration screen's **A/V & Emu -> Paths** page --
see [](configuration.md). Headless flags that take an explicit path
(`--screenshot-after` and friends) write exactly where they are told and
ignore all of this.

A `battmem.nvram` or `cd32-nvram.bin` left in the working directory by an
earlier Copperline keeps being used from there -- they hold real game saves,
so nothing is moved or abandoned behind your back.

For a self-contained installation, create an empty file named `portable.txt`
beside the Copperline executable (or beside the downloaded `.AppImage`) and
restart Copperline. That folder then becomes the host-data directory, and all
of the folders above -- plus gamepad calibration, keyboard mappings, the
default WHDLoad library and other per-user host data -- live inside it, so
the whole installation moves as one folder. Delete `portable.txt` to return
to the platform location above; Copperline does not move existing files in
either direction.

(controller-ports)=
## Controller ports

An Amiga has two game ports, and either accepts any controller. Copperline
models that: each port carries a device -- `mouse`, `joystick`, `cd32` pad,
`analogue` paddles, or `none`, and port 1 additionally `gamepad-mouse` --
set with `[input] port1`/`port2` in the
config (or `--port1`/`--port2`, or the launcher's *Input* tab). The default
is the stock wiring, a mouse in port 1 and a joystick in port 2 (a CD32 pad
on the CD32 profile). The runtime menu's **Port 1 Device** / **Port 2
Device** items hot-plug a different device live, exactly like swapping the
physical plug: the old device's lines release, and the port's quadrature
counters hold.

The host mouse drives the (lowest-numbered) port with a mouse plugged in
and feeds its JOYxDAT counters. Click the display (or press `Cmd+G` on
macOS or `Alt+G` on Linux/Windows) to capture the host mouse; the same
shortcut releases it. The click that takes the capture is a window
action and is not passed to the Amiga, so the first click that reaches
the guest is the first one aimed at it -- otherwise a single click on a
gadget arrives as two, close enough together to read as a double click.

While an overlay panel is open, host cursor motion is not fed to the
emulated mouse. Tool windows are not modal that way: with the debugger
or analyzer open, motion and clicks over the main window's display still
drive the Amiga, and the capture click and shortcut work as usual. A
panel or tool window opened while the mouse was captured borrows the
cursor and hands the capture back when the last of them closes, so a
visit to the debugger does not leave the machine uncaptured -- which
matters most in fullscreen, where there is no desktop to reach for. An
explicit `Cmd/Alt+G` release settles it the other way: the capture
stays off.

Uncaptured, host cursor motion over the display still drives the
emulated mouse, and `[input] mouse_sensitivity` scales it the same way
it scales captured motion. At the default setting the factor is 1.0 and
the pointer tracks the host cursor one-for-one.

`[input] mouse_capture` (`--mouse-capture`, the launcher's *Input* tab)
changes when the grab is taken: `auto` grabs whenever the window has the
focus and on entering fullscreen, so no host cursor is ever loose over
the display, and `manual` grabs only on the shortcut, leaving display
clicks to go straight to the Amiga. See
[Mouse capture](configuration.md#mouse-capture).

A USB gamepad drives the emulated digital joystick on whichever port one is
plugged into: directions through JOYxDAT, fire through /FIRx, and a second
button through POTxY/POTGOR. A `cd32` device speaks the CD32 serial button
protocol instead, including the red/blue/green/yellow and transport
buttons, on either port. An `analogue` device presents pot resistances on
the POTxX/POTxY pins; no live host device maps to it yet -- drive it with
`--pot-after` scripting or the control protocol's `input.analogue`.

A `gamepad-mouse` device is a mouse a gamepad moves as well as the host's
own; the machine still sees one mouse. The d-pad moves the pointer,
gathering speed while a direction is held, and the left stick moves it
proportionally where the pad has one. Fire is the left button, the second
button the right, and `mouse_sensitivity` scales both hands alike. It is
offered on port 1 only. While it is chosen the pad drives no joystick
port -- whichever port it would have driven falls to the keyboard until
the device is changed back.

Copperline can also emulate the joystick from the host keyboard. There are
two explicit input modes, and the active one is always shown by the gamepad
/ keyboard icon in the status bar (next to the volume control), so a "my
keys aren't working" surprise can be spotted and fixed at a glance:

- **gamepad** (the default): use only a recognised or
  [calibrated](#gamepad-calibration) gamepad; every keyboard key passes
  through to the Amiga. With no pad connected there is no joystick input.
  This is the no-surprise mode for interactive AmigaOS setup.
- **keyboard**: use the keyboard-joystick mapping so the joystick port
  works without a controller.

With joysticks (or pads) in *both* ports -- a two-player setup -- the
gamepad and the cursor-key mapping drive one port each, and the mode picks
which source gets the lower-numbered port. Whenever no physical pad is
present, a second keyboard mapping on the numeric keypad stands in for it,
so two players can share one keyboard.

With mice in both ports, the host mouse drives the lower-numbered one and
the cursor-key mapping drives the second as an emulated mouse (in
`keyboard` mode; in `gamepad` mode the second mouse is undriven and the
keyboard passes through). The same applies whenever the keyboard-routed
port carries a mouse: the mapping's keys become the pointer.

Click the status-bar icon to flip between them; the menu's **Joystick Input**
item and `Cmd+J` on macOS / `Alt+J` on Linux and Windows do the same. Set the
starting mode with `[input] joystick` in the config (or `--joystick MODE`, or
the launcher's *Input* tab).

The primary keyboard mapping is FS-UAE-compatible: cursor keys for
directions, and Right Ctrl, Right Alt or Left Ctrl for fire, with Left
Alt as the second button (the left-hand fire keys pair naturally with the
right-hand arrows, and compact keyboards often lack the right-side
modifiers). For CD32 pad buttons, `C` is red/fire, `X` is blue, `D` is
green, `S` is yellow, Return is play/pause, `Z` is rewind, and `A` is
forward. On a mouse port the same keys drive the pointer: cursor keys
move it, the fire keys are the left button, `X` or Left Alt the right,
and `D` the middle. The second (numpad) mapping is
`8`/`2`/`4`/`6` for directions, `0` for fire, `.` for the second button,
and numpad Enter for play. While a mapping owns its keys, they are not
sent to the Amiga keyboard.

Those are the defaults; every binding is editable from the menu's **Input
Mapping...** item (see [](#input-mapping)). A held fire button can also be
turned into a pulse train with the **Autofire** item or `[input]
autofire_hz`.

(gamepad-calibration)=
## Gamepad calibration

Most pads work with no setup at all: a controller found in the bundled
SDL_GameControllerDB (or identified by the platform driver) uses a fixed
standard layout. The d-pad and left stick drive the directions; the south
face button (A / Cross) is fire / CD32 red, east (B / Circle) is blue,
west (X / Square) is green, north (Y / Triangle) is yellow, Start is
play/pause, and the left and right shoulders or triggers are reverse and
forward. Select/Back and the guide button -- which no emulated control
uses -- open the pop-up menu, and Select *held* quits (both below).
Personal SDL mapping strings in the standard `SDL_GAMECONTROLLERCONFIG`
environment variable are honoured too.

Calibration is the per-pad override, and the only path for controllers
the database does not cover: push each control when prompted. This
records raw axis/button codes and directions, which makes any pad work
regardless of database coverage -- including pads with broken database
entries -- and handles inverted or odd axis layouts automatically. A
saved calibration always wins over the database for its pad.

```{figure} ../images/ui-preview-calibration.png
:alt: The gamepad calibration window
:width: 75%

The calibration window mid-flow. Skip covers pads without the CD32 extras.
```

Run it either from the menu ("Calibrate Gamepad...") -- which ends with a
live test of the finished bindings and a Save button that makes them live
immediately -- or from the terminal with `copperline --calibrate-gamepad`.
The steps are the four directions, fire (CD32 red), button 2 (CD32 blue),
the optional CD32 green/yellow/play/rewind/forward buttons, an optional
**Open menu** button, an optional **Quit Copperline** hotkey, and finally
the four directions again as optional **alternates**. Push a control to
bind it, or hold any control for about a second to skip a step the pad
has no control for; the four directions and fire cannot be skipped.

The alternates are for a pad with both a stick and a d-pad: bind the
stick to the first four steps and the d-pad to the alternates (or the
other way round) and either steers, as on the standard layout. Each
alternate simply ORs with its primary, so a pad with one set of
directions skips them and loses nothing. When a direction pair is bound
to the two ends of one stick axis, the stick's deflection is known too,
and a [`gamepad-mouse`](#controller-ports) device on that pad moves the
pointer at a speed that follows the stick rather than at the d-pad's
fixed pace.

The Quit hotkey is a host-side control: it never reaches the emulated
machine, so any spare pad button (Select, Start, a shoulder button) can
carry it without affecting the game. To guard against accidental presses
it must be *held* for about a second and a half -- an on-screen countdown
shows the hold in progress, and releasing early cancels it. It works
whenever the pad is connected -- even while the keyboard or another
device drives the joystick ports, and while the machine is paused or
powered off. On the default database layout Select/Back carries it: a
tap opens the menu and keeping it held quits, with the countdown drawn
over the menu; let go early and the menu simply stays open, where
**Quit** is the last row. A calibrated pad's Menu button works the same
way unless a separate Quit control was bound, which then takes the hold
to itself. Skip both steps to leave quitting to the menu row and the
keyboard shortcut.

The Menu button is the other host-side control: a press opens the pop-up
menu (or closes an open overlay panel), and from there the pad walks
whatever is up, as described under
[Keyboard and controller navigation](#keyboard-and-controller-navigation)
above. While the menu or a panel is open the pad stops driving the
emulated port, just as the keyboard does. On the default database layout
Select/Back and the guide button carry it; a calibration can put it
anywhere (or skip it). Held, it doubles as the Quit hotkey, as above.

Once every step is captured, pushing a control tests its binding and
holding one hands the panel's Save and Cancel buttons to the pad, so a
calibration can be finished without the mouse.

Calibrations are saved per controller UUID in
`~/Documents/Copperline/gamepads.toml`
(`%USERPROFILE%\Documents\Copperline\` on Windows, or beside the
executable in [portable mode](#quick-save-slots)). A calibration recorded by a
Copperline version that predates the bundled controller database can
resolve a stick direction reversed on a database-covered pad; the log
suggests recalibrating when it loads such a file, and recalibrating
once fixes it.

(input-mapping)=
## Input mapping

The keyboard also stands in for a controller, on two independent mappings so
one keyboard can drive a two-player setup (see [](#controller-ports)). The
menu's **Input Mapping...** item edits both.

```{figure} ../images/ui-preview-input-mapping.png
:alt: The input mapping window
:width: 75%

Editing the first keyboard mapping, with the Fire row armed for capture.
```

Pick the mapping with the **Controller 1** / **Controller 2** tabs, then
**Set** on a row and press the key to bind (Escape cancels that row without
closing the panel). **Clear** unbinds a control entirely, **Defaults**
restores the built-in layouts, and **Save** writes the map and applies it to
the running machine. Closing the window discards the edits.

A control may hold several keys, and they OR together: fire ships bound to
Right Ctrl, Right Alt, Left Ctrl and `C`, so compact keyboards without the
right-hand modifiers still work, and releasing one alias while another is
held keeps the button down. Binding a key removes it from wherever it was
before -- including from the other mapping -- so the two controllers can
never end up fighting over one key.

Saved maps live next to the gamepad calibrations, in
`~/Documents/Copperline/keymap.toml` (same per-platform locations as above).
Deleting the file restores the defaults.
