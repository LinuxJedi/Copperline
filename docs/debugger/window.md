# The debugger window

Press `Cmd+B` on macOS or `Alt+B` on Linux/Windows (or pick **Debugger**
from the status-bar menu) to pause the machine and open the debugger
window. Closing it restores the pause state from before it opened.
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

The `R`/`S`/`F` keys work whenever the hex box is unfocused (while it is
focused they are hex input). **Run to $** is bounded by a 2M-instruction
budget so a never-reached address cannot wedge the UI; if the budget runs
out, the debugger says so and stays paused. If the CPU is sitting in a
`STOP`, stepping fast-forwards device time to the interrupt that wakes it,
exactly as the live core would.

**Frame** is the tool for raster work: combined with the Chipset and Copper
tabs it lets you single-step a Copper effect one frame at a time and watch
the register state the beam will replay.
