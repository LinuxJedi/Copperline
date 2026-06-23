# The debugger window

Press `Cmd+B` on macOS or `Alt+B` on Linux/Windows (or pick **Debugger**
from the status-bar menu) to pause the machine and open the debugger
tool window alongside the emulated display. Closing it restores the pause
state from before it opened. The debugger and frame analyzer are independent
tool windows, so both can stay open while you compare CPU/chipset state with
the captured bus trace.
Everything the debugger shows comes from
side-effect-free peeks -- inspecting memory or registers never disturbs the
emulated machine -- and stepping drives the same cycle-exact core as normal
execution.

```{figure} ../images/ui-preview-debugger.png
:alt: The debugger window on the CPU tab
:width: 90%

The CPU tab: register file, live disassembly, and the transport controls.
```

## Tabs

**CPU** shows the PC and SR (with decoded supervisor/IPL/CCR flags), the
D0-D7/A0-A7 register file, and a live 68000 disassembly that follows the
PC, with the current instruction highlighted. Type a hex address in the
`$` box and press Enter to *pin* the disassembly elsewhere; empty the box
and press Enter to follow the PC again.

**Chipset** decodes the live custom-chip state bit by bit: the beam
position and frame counter, DMACON / INTENA / INTREQ with bit names,
Copper state (COP1LC/COP2LC/COPPC), the display window and fetch registers
(BPLCONx, DIWSTRT/STOP, DDFSTRT/STOP, modulos), the bitplane and sprite
pointers, and the full palette.

**Copper** disassembles the Copper list from COP1LC -- MOVE/WAIT/SKIP with
decoded targets and positions -- and highlights the instruction at the
current Copper fetch address.

**Memory** is a hex/ASCII dump, 256 bytes per page. Type a hex address in
the `$` box and press Enter to jump there; the `<` and `>` buttons page by
256 bytes.

**Break** manages breakpoints and watchpoints (next section) and shows the
reason for the last stop.

```{figure} ../images/ui-preview-debugger-break.png
:alt: The Break tab
:width: 90%

The Break tab with a PC breakpoint, a memory watchpoint, and a
chipset-register watch armed.
```

## Breakpoints and watchpoints

On the Break tab, type an address into the `$` box and toggle any of:

- **Break** -- a PC breakpoint. The machine stops *before* the instruction
  at that address executes.
- **Watch** -- a memory word watchpoint. The machine stops when the word
  changes, whichever bus master wrote it (CPU, Copper, or blitter); the
  current value is shown live in the list.
- **Reg** -- a chipset-register write watch. `96` and `DFF096` both mean
  DMACON. The machine stops on *every* write, CPU or Copper, and reports
  the writer and beam position.

**Clear all** removes everything. Breakpoints and watchpoints stay armed
when the window is closed: a hit pauses the machine, reopens the debugger
on the Break tab with the reason highlighted, and shows it as an on-screen
message.

## Transport controls

| Control | Key | Effect |
|---|---|---|
| Run / Pause | `R` | Resume or pause the machine |
| Step | `S` | Execute exactly one instruction |
| Frame | `F` | Run to the next video frame and re-render the display |
| Run to `$` | -- | Run until the PC reaches the address in the box |
| &lt; Frame | -- | Step one video frame *backward* |
| &lt; Step | -- | Step one instruction *backward* (see [](reverse)) |
| &lt; Run | -- | Run *backward* to the previous breakpoint hit |

The `R`/`S`/`F` keys work whenever the hex box is unfocused (while it is
focused they are hex input). **Run to $** is bounded by a 2M-instruction
budget so a never-reached address cannot wedge the UI; if the budget runs
out, the debugger says so and stays paused. If the CPU is sitting in a
`STOP`, stepping fast-forwards device time to the interrupt that wakes it,
exactly as the live core would.

**Frame** is the tool for raster work: combined with the Chipset and Copper
tabs it lets you single-step a Copper effect one frame at a time and watch
the register state the beam will replay.

## Frame Analyzer pane

Pick **Frame Analyzer...** from the status-bar menu to pause the machine and
open the chip-bus frame analyzer in a separate tool window, leaving the
normal emulated display visible in the main window. It can remain open next
to the debugger window. The analyzer shows the whole captured Agnus beam
frame, not just the TV-presented display. The trace includes vertical and
horizontal overscan, blanking, and the visible display window.

The main heatmap is indexed by beam position: X is `hpos` colour clocks and Y
is `vpos` lines. Each cell records the chip-bus owner for that colour clock:
refresh, bitplane, sprite, disk, audio, Copper, blitter, CPU, or idle. The
white outline marks the framebuffer display area that Copperline captured for
presentation. Register-write markers show CPU, Copper, and interrupt-time
custom-register writes at their beam positions.

Click or drag across the heatmap to select a beam slot. The cursor keys nudge
the selector one colour clock or line at a time. The lower strip expands that
selected scanline, so horizontal DMA contention in overscan is easier to
inspect. The right-hand counters summarize total colour clocks per owner, the
percentage of busy-blitter time that the blitter actually received, and which
owners consumed cycles while the blitter was waiting.

The pane has the same transport rhythm as the debugger:

| Control | Key | Effect |
|---|---|---|
| Run / Pause | `R` | Resume or pause while continuing to collect frame traces |
| Frame | `F` | Run exactly one frame and show the completed trace |

Opening the pane starts a partial trace immediately; pressing **Frame**
captures a clean full frame. Closing it restores the run/pause state selected
inside the pane and disables the tracing hot path.
