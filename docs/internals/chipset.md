# Chipset modules

Each custom chip is a module under `src/chipset/`, owned by the `Bus` and
stepped in emulated time. Unit tests live inline in each module's
`#[cfg(test)] mod tests` block; the suites are large and are the best
specification of the modelled behaviour.

## Agnus (`agnus.rs`)

Agnus owns the beam: `vpos`/`hpos` counters advanced per colour clock, PAL
(313 lines, 227 CCK/line) and NTSC (263 lines with long/short line
alternation) geometry, the long-field flag for interlace, and VPOSR/VHPOSR.
It also owns DMACON and the display-fetch machinery: the bitplane fetch
plan for the current line is computed from DDFSTRT/DDFSTOP, the plane
count, resolution, and FMODE, producing the per-slot fetch pattern the
[arbitration model](timing) consumes. The fetch sequencer is anchored by
the DDFSTRT comparator, then each fetch block/unit uses the BPLCON0 value
visible at that block's first cycle. A mid-row BPLCON0 plane-count change
therefore cannot retroactively fetch earlier words, but it can add or
remove planes for later blocks in the same row. The sequence completes
whole fetch units: DDF registers have finer
granularity than the unit in hi-res/super-hi-res and wide-FMODE modes, so
a DDFSTOP landing mid-unit extends the fetch through the unit starting
at-or-after it (`agnus::bitplane_fetch_blocks`; the CDTV trademark
screen's hi-res $64/$A8 window fetches 20 words per row, not the truncated
18). Lo-res OCS has an extra asymmetry: DDFSTRT is rounded to the 8-CCK
fetch-block start, but DDFSTOP keeps its 4-CCK comparator granularity, so
a stop in the second half of a block still includes that block. A $4A/$B6
window therefore fetches 15 words per row, matching five 30-byte
interleaved planes. Wide-FMODE units (16/32 CCK) use the same rule rather
than moving DDFSTRT down to an absolute grid. In lo-res, the plane-order
slots for a wide unit are packed into the unit's first eight CCKs; the
remaining CCKs are free for other bus users. If a bitplane fetch block
occupies sprite 7's late DMA slot at $30, sprite 7 DMA is blocked for that
line; the condition is derived from the fetch-block sequence, not from a
single DDFSTRT value. SANITY Roots II's AGA 256-colour effects are
regression examples for both sides of this: the hi-res FMODE=3 pictures
need raw-DDFSTRT unit rounding to preserve their 40-word rows, and the
lo-res FMODE=3 landscape needs packed first-eight CCK plane slots instead
of spreading those slots across the 32-CCK unit.

Agnus revisions are modelled independently of Denise (machines shipped
mixed): OCS (8370/8371), ECS 8372A (1M chip RAM reach), ECS 8375 (2M), and
AGA Alice (2M, HRM IDs $23/$33). The ECS Agnus adds DIWHIGH and the
implemented subset of BEAMCON0 (PAL/VARBEAMEN/LOLDIS/HARDDIS and friends);
Alice adds the FMODE wide-fetch latch, which scales the bitplane and
sprite fetch quanta (FMODE=0 stays byte-identical to the OCS/ECS slot
timing).

Sprite DMA retains its latched POS/CTL descriptor independently from the
SPRxPT registers. If software fetches a descriptor off-screen, then writes
SPRxPT before the sprite's VSTART, Agnus retargets the data pointer for
the retained descriptor instead of requiring a new descriptor at the new
address. The frame-start replay path mirrors this by replaying off-screen
DMACON and SPRxPT writes in beam order before rendering the visible field.

A modelling note that catches people out: OCS lo-res with BPU=7 is an
overprogrammed mode. Denise still decodes six BPLDAT latches, but Agnus
only schedules four DMA streams, so planes 5 and 6 display whatever was
last latched -- this is hardware behaviour, not a bug.

## Copper (`copper.rs`)

The Copper decodes MOVE/WAIT/SKIP and executes on its beam-locked fetch
cadence (see [](timing)). It runs from Agnus beam
time, is gated by DMACON's COPEN, restarts from COP1LC each frame, and its
register writes are recorded as beam events for the renderer.

## Blitter (`blitter.rs`)

A scheduled per-DMA-slot engine with the hardware per-word channel
sequences for normal, line, and fill modes; see [](timing). Normal-mode
A/B barrel-shifter carry is a datapath latch and survives BLTSIZE row
boundaries; first/last masks, area-fill state, and modulos remain row
scoped. ECS adds BLTSIZV/BLTSIZH for larger blits.

## Paula (`paula.rs`)

Paula owns the interrupt system (INTENA/INTREQ, delivered through the
modelled 68000 recognition latency), serial, and audio:

- **Audio**: four DMA channels, each with location/length/period/volume,
  a period accumulator clocked at CCK rate, and the hardware's one-word
  fetch-ahead (audible with short periods). Channel interrupts fire on
  buffer completion; LEN=0 plays a full 65536-word block, as on hardware.
  Output is mixed in emulated time to stereo with the LED filter, then
  resampled at the host boundary.
- **Serial**: SERDAT through a one-word transmit buffer and a timed shift
  register to stdout; SERDATR reports TBE/TSRE/RBF. DiagROM's diagnostic
  stream arrives this way.
- **Disk registers**: DSKLEN/DSKBYTR/DSKSYNC/DSKDAT and the disk-block
  interrupt, fed by the floppy controller below.
- **Pots**: POTGO/POTGOR counters at the hardware 512-CCK rate (the second
  mouse/joystick button path).

## Denise (`denise.rs`)

Palette (32 12-bit entries as seen by OCS/ECS; the store is the AGA
256-entry layout of high/low nibble-plane pairs giving 25-bit colour, with
Lisa COLORxx writes routed through BPLCON3 BANK/LOCT banking), BPLCON0-4,
display window (DIWSTRT/DIWSTOP, ECS DIWHIGH), sprite
position/control/data registers, and CLXCON/CLXDAT collision detection
(CLXCON2 extends it to planes 7-8 on Lisa). Denise revisions: OCS 8362,
ECS 8373, AGA Lisa (DENISEID $00F8). The AGA decode adds 8 bitplanes,
HAM8, the BPLCON4 BPLAM pixel-index XOR mask, and the OSPRM/ESPRM sprite
palette banks. The two BPLCON4 fields are on different Lisa timing paths:
the low byte that selects sprite palette bases (ESPRM/OSPRM) reaches sprite
colour lookup in the colour-output domain, while the high-byte BPLAM XOR
continues on the normal bitplane/control path. AGA also widens dual
playfield: OCS/ECS split six bitplanes into two three-bit fields (PF1 =
planes 1/3/5, PF2 = planes 2/4/6), while Lisa extends each field to four
bits by feeding bitplane 7 into PF1 and bitplane 8 into PF2, so a 7-8
plane dual playfield addresses palette entries 8..15 per field. The extra
bits are gated on the AGA revision; pre-AGA chips never carry bitplanes
7/8 and keep the exact three-bit decode. Denise state is not rendered live
-- writes become beam events that the [video pipeline](video) replays.

The ECS DIWHIGH high bits only stay in force until the next DIWSTRT or
DIWSTOP write, which re-arms the OCS-implicit high bits derived from the
low DIWSTRT/DIWSTOP values. Software that programmed a wide window through
DIWHIGH and then touches DIWSTRT/DIWSTOP falls back to the implicit
window, so the replay must drop the stale DIWHIGH on those writes rather
than hold it.

DIWSTRT value zero is still a real Denise comparator position. The emulator
only treats the display window as unprogrammed when DIWSTRT and DIWSTOP are
both zero; a zero start paired with a non-zero stop opens the window at beam
zero and can expose deep overscan.

## CIA (`cia.rs`)

A small 8520 model used for both CIAs: I/O ports, the
interval timers with cascading and underflow pulses, the 24-bit TOD
counters (VSYNC-clocked on CIA-A, HSYNC on CIA-B) with latch and alarm
semantics (including the hardware quirk that a reset alarm is $000000),
and the ICR with its read-clears behaviour.

CIA-A carries /OVL (the reset-time ROM overlay at `$0`), the keyboard
serial port (SDR/ICR with the KDAT handshake and an emulated
keyboard-controller pacing delay), and the fire-button lines. CIA-B
carries the floppy control lines (motor, select, side, step) and the FLAG
input pulsed by the disk index.

## Floppy (`floppy.rs`)

The floppy subsystem is track-timed: a drive has a rotational position,
and data under the head right now is what disk DMA sees. Track stepping
pays settle time, direction reversals cost more, and the index pulse fires
once per revolution into CIA-B FLAG. Reads assemble MFM bitstreams from
the 11-sector AmigaDOS track layout; DSKSYNC matching, word-at-a-time
DSKDAT, and DMA into chip RAM behave as Paula documents. Non-WORDSYNC read
DMA drains Paula's recovered 16-bit disk word phase even when DSKLEN is
armed between disk-word boundaries; WORDSYNC is the explicit mode that
realigns framing to a matched sync word before transfer. Supported image
formats: ADF (read/write), gzip ADZ, DMS (decompressed by `dms.rs`), UAE
extended ADF, and read-only SCP flux images.

The synthesized drive sounds ([](../guide/configuration)) are driven by
this model's real state transitions -- motor spin-up, seeks, the
empty-drive poll click.

## Known AGA/ECS gaps and non-goals

Most ECS and AGA behaviour is implemented (the register notes above and
[](video)); the chipset gaps that remain are:

- **Sub-unit AGA DDF stop effects** beyond whole-unit completion are not
  modelled; the current model starts from DDFSTRT and rounds DDFSTOP
  through complete FMODE units.
- **Live (beam-timed) collisions** stay on the 6-plane decode: CLXCON2 is
  interpreted in the rendered collision path but not yet in the beam-timed
  `COLLISIONS_AGA_DECODE` path.
- **True 35 ns SuperHires sprite** output is not modelled -- SPRES upgrades
  sprite resolution, but the compositor does not place sprites on the SHRES
  pixel grid.
- The vAmigaTS ECS register-readback sweep has not been run against a local
  checkout; readback is pinned by unit tests meanwhile.

Deliberate non-goals, recorded so they are not re-investigated: A2024 /
UHRES dual-scan display (a one-time "not emulated" warning is kept),
genlock ZD output beyond register storage, and AGA "double CAS"
memory-timing fidelity beyond what `timing-test/` measurements justify.
