# Chipset modules

Each custom chip is a module under `src/chipset/`, owned by the `Bus` and
stepped in emulated time. Unit tests live inline in each module's
`#[cfg(test)] mod tests` block; the suites are large and are the best
specification of the modelled behaviour.

## Agnus (`agnus.rs`)

Agnus owns the beam: `vpos`/`hpos` counters advanced per colour clock, PAL
(313 lines, 227 CCK/line) and NTSC (263 lines with long/short line
alternation) geometry, the long-field flag for interlace, and VPOSR/VHPOSR.
It also owns DMACON and the display-fetch machinery: for FMODE=0 fetches
the per-line fetch table comes from the DDF sequencer flop model
(`src/chipset/ddf_sequencer.rs`; see the [arbitration model](timing) for
the flop semantics - comparator edges, stop drain through a final
modulo-applying unit, cross-line run carry, OCS/ECS rule differences).
Each fetch unit uses the BPLCON0 value the sequencer sees at that point,
so a mid-row plane-count change cannot retroactively fetch earlier words,
but it can add or remove planes for later units in the same row; word
addressing stays unit-based. The DDF register value is masked to the Agnus
revision's comparator precision (OCS keeps 4-CCK precision; ECS/AGA keep
2-CCK precision), and a DDFSTOP landing mid-unit extends the fetch through
the unit starting at-or-after it plus the drain unit (the CDTV trademark
screen's hi-res $64/$A8 window fetches 20 words per row, not the truncated
18). In lo-res OCS, bit 2 of DDFSTRT and DDFSTOP remains visible
to the 8-CCK fetch-unit count: $34/$D4 fetches 21 words, $28/$D4 fetches
23, and $4A/$B6 fetches 15. Wide-FMODE units (16/32 CCK) use the same rule
rather than moving DDFSTRT down to an absolute grid. In
lo-res, the plane-order slots for a wide unit are packed into the unit's
first eight CCKs; the remaining CCKs are free for other bus users. If a
bitplane fetch block that started before sprite 7's late DMA slot is still
active at $30, sprite 7 DMA is blocked for that line; a DDFSTRT value of
$30 itself matches on the following odd cycle and does not steal the already
decided sprite slot. The condition is derived from the fetch-block sequence,
not from a single DDFSTRT value. SANITY Roots II's AGA 256-colour effects are
regression examples for both sides of this: the hi-res FMODE=3 pictures need
raw-DDFSTRT unit rounding to preserve their 40-word rows, and the lo-res
FMODE=3 landscape needs packed first-eight CCK plane slots instead of
spreading those slots across the 32-CCK unit.

Like SPRxPT below, each BPLxPT is one live counter: bitplane fetches
advance it and the end-of-line modulo adds to the full pointer, carrying
across the 16-bit register boundary, and it is never reloaded at vertical
blank -- software rewrites the pointers each field. A BPLxPTH/PTL write
replaces only that half of the DMA-advanced value, which programs exploit
to flip 8-bitplane double buffers with half the Copper writes: the modulo
is sized so the end-of-frame carry lands the high half on the next
buffer, and only the PTL half is rewritten per frame.

Agnus revisions are modelled independently of Denise (machines shipped
mixed): OCS (8370/8371), ECS 8372A (1M chip RAM reach), ECS 8375 (2M), and
AGA Alice (2M, HRM IDs $23/$33). VPOSR bits 8-14 report the chipset ID:
$00 on OCS, $20/$30 (PAL/NTSC) on every ECS Agnus regardless of revision,
and the Alice IDs above. The ECS Agnus adds DIWHIGH and the
implemented subset of BEAMCON0 (PAL/VARBEAMEN/LOLDIS/HARDDIS and friends);
Alice adds the FMODE wide-fetch latch, which scales the bitplane and
sprite fetch quanta (FMODE=0 stays byte-identical to the OCS/ECS slot
timing). The fetch phase supplies the low pointer bits: 32/64-bit fetches
therefore alias words when BPLxPT/SPRxPT is not naturally aligned, and the
FMODE `10` page mode duplicates the first 16-bit word while advancing the
pointer by the full 32-bit width. Fetch bandwidth also bounds valid AGA plane
counts: FMODE0 permits 8/4/2 planes in lo-res/hi-res/SHRES, FMODE1-2 permits
8/8/4, and FMODE3 permits eight in every resolution. An overprogrammed count
fetches no bitplanes rather than clamping. FMODE's BSCAN2 and SSCAN2 bits
repeat bitplane and sprite data on successive display lines. SSCAN2 also masks
the high bit of Lisa's sprite horizontal comparator: HSTART `$100..$1FF`
aliases `$000..$0FF` while the bit is active. DblPAL/DblNTSC modes rely on that
alias for the Workbench pointer.

The CPU sees the reach too: the motherboard decode (Gary and equivalents)
routes the whole $000000-$1FFFFF window to Agnus, which decodes only as
many address bits as its DRAM reach, so the fitted chip RAM image repeats
across the window. A 512K OCS machine mirrors chip RAM at
$080000/$100000/$180000, and the 8372A (A20 unused) repeats its 1M image
at $100000, while addresses past the fitted RAM inside one image select
no DRAM bank and stay open bus ($080000-$0FFFFF on an 8372A with 512K
fitted). Kickstart's chip sizing detects the wrap and still reports the
fitted size. Action Replay freeze-disk loaders are a regression example:
they park the supervisor stack at $100000 on a 512K OCS machine and rely
on the pushes landing at the top of chip RAM.

Sprite DMA is modelled at the register level, the way the chips work:
there is no separate "descriptor" concept. Each channel keeps Agnus-side
copies of its SPRxPOS/SPRxCTL words and the vertical comparator values
derived from them, updated identically by DMA fetches and by CPU/Copper
pokes (POS supplies the vertical start's low bits, CTL the high bit, the
whole vertical stop, and disarms the display latches). At each line start
the comparators run: passing vstart sets the channel's DMA flip-flop,
passing vstop clears it -- even when SPREN is off, which is what leaves a
sprite dead until the next field when software disables DMA across its
vstop line. Writes landing on the matching line re-evaluate immediately.

A sprite line's two DMA slots ($15+4N and $17+4N) are evaluated at their
own colour clocks. On the channel's vstop line the slots fetch the next
control words (POS in the first slot, CTL in the second); on other lines
an enabled channel fetches DATA and DATB, so chip RAM rewritten between
the two slots is seen by DATB but not DATA. The vertical-blank reset
(PAL line $19, NTSC line $14 -- sprite DMA is inhibited above it) forces
every channel's vstop to that line, which is how each field's first
control-word fetch happens; software reloads SPRxPT each vblank to point
it at the sprite list. An "inverted" pair (vstop before vstart, or a
$0000/$0000 terminator) simply parks the comparators on values the beam
has passed or never shows: the terminator decodes to vstart=vstop=0 and
the channel stays silent until the next field's reset.

DMACON's SPREN is sampled by each DMA slot individually, so a mid-line
edge fetches exactly one word of the pair. SPRxPT advances only on words
actually fetched: a skipped slot leaves the pointer behind and the stream
shifts accordingly. The display line assembles from the fetched words
plus the stale latch on a skipped side; a missed DATA slot never arms the
sprite, and an armed channel with DMA off keeps redisplaying its latches
at the current POS/CTL decode until a CTL fetch or poke disarms it (the
vAmigaTS sprena/sprdis families' vertical bars).

Because SPRxPT only moves on fetches, a channel that consumed its list
leaves the pointer parked at the DMA frontier past it, and the next
field's replay seeds from that frontier rather than from the stale
last-written value; programs must reload SPRxPT every field (normally
from the Copper) to keep a sprite displayed. The frame-start replay path
replays off-screen DMACON, SPRxPT and SPRxPOS/CTL writes in beam order
before rendering the visible field. The whole per-channel register state
is chip state: save states serialize and restore it directly.

A modelling note that catches people out: OCS lo-res with BPU=7 is an
overprogrammed mode. Denise still decodes six BPLDAT latches, but Agnus
only schedules four DMA streams, so planes 5 and 6 display whatever was
last latched -- this is hardware behaviour, not a bug.

## Copper (`copper.rs`)

The Copper decodes MOVE/WAIT/SKIP and executes on its beam-locked fetch
cadence (see [](timing.md)). It runs from Agnus beam
time, is gated by DMACON's COPEN, restarts from COP1LC each frame, and its
register writes are recorded as beam events for the renderer.

## Blitter (`blitter.rs`)

A scheduled per-DMA-slot engine with the hardware per-word channel
sequences for normal, line, and fill modes; see [](timing.md). Normal-mode
A/B barrel-shifter carry is a datapath latch and survives BLTSIZE row
boundaries; first/last masks, area-fill state, and modulos remain row
scoped. ECS adds BLTSIZV/BLTSIZH for larger blits.

Normal blits snapshot their A/B source words at BLTSIZE while C reads stay
live. An ordered overlay records D writes so overlapping A/B reads can see
the blit's own output. Ascending, zero-modulo transfers skip that overlay
when the complete D byte span is disjoint from both enabled source spans
and every span maps directly into populated chip RAM. Descending, strided,
wrapping, aliased, or potentially overlapping transfers keep the general
path. The snapshots, bus slots, and progressive D writes are unchanged.
Save states retain the existing overlay fields: older active states with
an overlay still resume, while new proven-disjoint blits save an empty one.

## Paula (`paula.rs`)

Paula owns the interrupt system (INTENA/INTREQ, delivered through the
modelled IPL-pin pipe and 68000 boundary sampling), serial, and audio.

The IPL pipeline (`DEFAULT_IRQ_LATENCY_CCK`, 5 cck) models the propagation
delay from interrupt assertion through the level encoder to the CPU IPL pins:

- **Independent pipelines**: Each interrupt source propagates independently to
  the IPL pins, ensuring an asserted interrupt does not delay a pending
  higher-priority interrupt.
- **Waking from STOP**: A halted CPU waiting in `STOP` is awakened specifically
  when the signal exits the pipeline rather than at arbitrary event boundaries.
  This prevents interrupt handling jitter across raster lines, ensuring
  cycle-accurate timing for demos that update sprite descriptors immediately
  following the vertical blank (such as *Spooky Town* by Ghostown).

The rest of Paula:

- **Audio**: four channels running the HRM per-channel state machine
  (states 000/001/101/010/011): AUDxDAT arrivals, the period counter,
  and DMACON edges drive the transitions, whether the data comes from
  the channel's DMA slot or a CPU write (Paula cannot tell them apart,
  so a CPU AUDxDAT poke during DMA playback counts against the length
  counter like a fetch). DMA start-up performs two fetches -- the first
  from the stale pointer, discarded, raising the channel interrupt and
  resetting the pointer to AUDxLC -- before output begins; the length
  rollover reloads pointer/length at the final-word fetch and interrupts
  at the following word start. State-machine DMA requests transfer to
  Agnus at each line end and the fixed audio slots service them on the
  next line regardless of the DMACON bits (a request posted by a brief
  AUDxEN pulse is still fetched after the channel is switched off, which
  kicks the channel into free-running IRQ-mode output at the AUDxPER
  cadence -- the software "period timer" idiom, vAmigaTS pertimer1).
  Clearing AUDxEN while the channel is outputting is not sampled at the
  DMACON write: Paula only re-evaluates AUDxON at the word-start boundary
  (the 011 period event, which idles the channel when AUDxON is low and
  the channel interrupt is pending). A clear followed by a re-enable
  before that boundary is therefore missed entirely and playback
  continues from the live pointer rather than restarting from AUDxLC
  (the 2c1.adf regression, issue #74; vAmiga idles immediately on the
  clear and gets this case wrong). Only the DMA start-up states 001/101,
  which have not begun output, idle at the write.
  ADKCON attach modes feed the fetched words to the next channel's
  volume latch at word starts and period latch mid-word. LEN=0 plays a
  full 65536-word block, as on hardware. Output is mixed in emulated
  time to stereo with the LED filter, then resampled at the host
  boundary.
- **Serial**: SERDAT through a one-word transmit buffer and a timed shift
  register to stdout; SERDATR reports TBE/TSRE/RBF. DiagROM's diagnostic
  stream arrives this way.
- **Disk registers**: DSKLEN/DSKBYTR/DSKSYNC/DSKDAT and the disk-block
  interrupt, fed by the floppy controller below.
- **Pots**: POTGO/POTGOR's integrating converter is H-sync-clocked. START
  discharges the four capacitors and holds their counters at zero for 8 PAL or
  7 NTSC lines; each input then increments until its RC charge crosses the
  comparator threshold. The fixed-threshold transfer is linear in resistance
  (`t = -RC ln(1-Vt/Vcc)`), calibrated so the HRM's 528 kOhm maximum reaches
  count 255 (the recommended 470 kOhm paddle reaches 227). Each channel
  latches independently; a disconnected, grounded-button, or output-low pin
  remains below threshold and wraps, while output-high charges immediately
  unless the external button holds it low. POTGOR's production-Paula ID field
  is explicitly zero. Software that writes POTGO=$FFFF to read POTxDAT back as
  ~0 (e.g. the Bitmap Brothers input poll) depends on the output-high path.

## Denise (`denise.rs`)

Palette (32 12-bit entries as seen by OCS/ECS; the store is the AGA
256-entry layout of high/low nibble-plane pairs giving 24-bit colour plus
the genlock transparency (T) bit, with Lisa COLORxx writes routed through
BPLCON3 BANK/LOCT banking), BPLCON0-4,
display window (DIWSTRT/DIWSTOP, ECS DIWHIGH), sprite
position/control/data registers, and CLXCON/CLXDAT collision detection.
On Lisa, CLXCON2 extends the plane match to planes 7-8 in both the rendered
and live beam-timed collision paths. Denise revisions: OCS 8362,
ECS 8373, AGA Lisa (DENISEID $00F8). The AGA decode adds 8 bitplanes,
HAM8, the BPLCON4 BPLAM pixel-index XOR mask, and the OSPRM/ESPRM sprite
palette banks. The two BPLCON4 fields are on different Lisa timing paths:
the low byte that selects sprite palette bases (ESPRM/OSPRM) reaches sprite
colour lookup on an earlier sprite palette-control path than ordinary COLORxx
palette writes, while the high-byte BPLAM XOR continues on the normal
bitplane/control path. AGA also widens dual
playfield: OCS/ECS split six bitplanes into two three-bit fields (PF1 =
planes 1/3/5, PF2 = planes 2/4/6), while Lisa extends each field to four
bits by feeding bitplane 7 into PF1 and bitplane 8 into PF2, so a 7-8
plane dual playfield addresses palette entries 8..15 per field. The extra
bits are gated on the AGA revision; pre-AGA chips never carry bitplanes
7/8 and keep the exact three-bit decode. Denise state is not rendered live
-- writes become beam events that the [video pipeline](video.md) replays.

BPLCON2's PF1P/PF2P priority codes behave differently on the two chip
generations, and the split is where the evidence is. Denise draws a dual
playfield field transparent when its code is programmed out of range (5-7):
the winning field collapses to the background instead of revealing the
field behind it. That is photographed on an A500 (vAmigaTS
Denise/Registers/BPLCON0/invprio1 runs PF2 code 7 and the real machine
shows background between the bars).

Lisa does not inherit it. Alfred Chicken runs its whole in-game display at
BPLCON2 = 0x003F -- both codes 7 -- and draws an eight-plane dual playfield
on real AGA hardware, which the Denise rule would blank to the background
colour. The quirk reached us from an OCS/ECS-only reference, so it never
carried evidence about Lisa in the first place; WinUAE, which does model
AGA, resolves the playfield colour from the plane bits alone and uses the
codes only to mask sprites. On both chips the code still saturates in the
sprite comparison, where it counts the sprite pairs passing in front of the
playfield: 101/110/111 behave as 100. No AGA photo of invprio1 exists yet
to confirm Lisa ignores out-of-range codes entirely rather than differing
some other way.

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

Vertically the window is a flop, not a level comparison: it is set when the
beam line matches DIWSTRT.V and reset when it matches DIWSTOP.V (reset wins
a tie), with the comparators running against the live register values. A
mid-frame rewrite of the window therefore arms the comparators for a later
line but never re-opens a window whose old DIWSTOP already matched this
frame. That ordering is what makes the common copper screen-split idiom
safe on real hardware: close the main screen at line N, spend the rest of
line N writing the lower band's DIW, mode and bitplane pointers, and let
the new DIWSTRT match open the band at line N+1 (Kang Fu CD32's status bar
is the regression example; a level test resumed fetching in the tail of
line N and raced the pointer writes). The flop is evaluated at every line
start and after DIW writes; the video standard's fixed line 312 (PAL) or
262 (NTSC) forces a reset (winning over a DIWSTRT match on that line), so
a window whose DIWSTOP line the beam never reaches stays open only to the
end of its own frame and the next frame's top starts closed. On the
hardwired beam schedule the comparator is the fixed standard line,
exactly as vAmiga models it: an interlaced short field ends one line
earlier and is not force-reset. VARBEAMEN bypasses the hardwired sync
decode and its programmable frame may end before line 312 or run past it
mid-frame (DblPAL), so there the reset follows the programmable frame's
last line instead. The `vdiwprobe-flop` golden render pins the
progressive-PAL case, where the fixed line and the frame's last line
coincide; no hardware evidence yet pins the reset line on short fields
or programmable totals. The
latch is part of the save state: it is history-dependent, and control
sessions can snapshot mid-frame where no register-derived reconstruction
is exact.

Setting `DIWSTRT.V == DIWSTOP.V` on the same scanline (often below the visible
area via `DIWHIGH`) creates a vertical display window that never opens,
blanking bitplanes for the entire frame while buffers are redrawn. Sprites
remain unaffected because they do not use the bitplane vertical comparator,
allowing backdrop sprites (`BPLCON3.BRDRSPRT`) to remain visible across the
blanking interval (e.g. *Nexus 7* scene transitions with
`DIWSTRT.V = DIWSTOP.V = 301`). The level-based window test treats this tie as
closed; `vdiwprobe-empty` validates this whole-frame blanking behavior
(verified against vAmiga).

If the registers are rewritten to matching values mid-frame after an earlier
`DIWSTRT` match already opened the window, the window remains open until the
beam reaches the shared comparator line, where reset takes precedence. The
renderer uses DMA capture history as the authoritative vertical window state
and falls back to level tests only when no DMA rows were fetched
(`vdiwprobe-tieopen`).

The interlace long-frame latch (VPOSR bit 15) auto-toggles only while
BPLCON0 LACE is set; outside interlace it holds its value - the power-on
state is set, so progressive fields normally read LOF=1, and a value
written through VPOSW persists until the next laced toggle. Non-laced
frame timing always uses the long frame length regardless of the latch.
The held power-on state matters for the phase: the first field after
software enables LACE toggles to a short field, as on real hardware,
which is the absolute field order that software pairing its per-field
images blindly (without reading VPOSR) was authored against.

## CIA (`cia.rs`)

A small 8520 model used for both CIAs: I/O ports, the
interval timers with cascading and underflow pulses, the 24-bit TOD
counters (VSYNC-clocked on CIA-A, HSYNC on CIA-B) with latch and alarm
semantics (including the hardware quirk that a reset alarm is $000000),
and the ICR with its read-clears behaviour. The /IRQ pin follows an
INT-flag edge one E-clock later, armed per interrupt source (timers land
on the same E-cycle their underflow was observed; the other sources take
the extra cycle), matching the CIASetInt latency observable from the CPU.

CIA-A carries /OVL (the reset-time ROM overlay at `$0`), the keyboard
serial port (SDR/ICR with the KDAT handshake and an emulated
keyboard-controller pacing delay), the fire-button lines, and the
Centronics parallel data port: its port-B access pulse (`PC`) is the
Centronics `/STROBE`, so the bus samples the physical PRB pins without
creating another access, forwards one strobe to the attached parallel
peripheral, and feeds an accepted byte's `/ACK` edge back through CIA-A
FLAG. With no peripheral attached the line remains unacknowledged, like an
unplugged cable. CIA-B carries the floppy control lines (motor, select,
side, step), the FLAG input pulsed by the disk index, the parallel
status lines (BUSY/POUT/SEL on port A bits 0-2), and the RS-232 control
lines: /DSR, /CTS, and /CD inputs on port A bits 3-5, driven through the
inverting 1489 receivers by whatever the serial port is wired to on the
host (see [peripherals](peripherals.md#serial-sink)), and the /RTS and
/DTR outputs on bits 6-7. PB6/PB7 pulse-output mode
holds the selected pin low for one E-clock; reading PRB observes the pulse
without shortening it.

CIA timing is resolved on the E-clock grid (one tick per five colour clocks),
which is the smallest phase the current CPU/bus boundary exposes. Timer
underflow and delayed-source IRQ differences are preserved on that grid, as
are the TOD mid-ripple alarm quirk and one-E PB pulse width. Transitions wholly
inside one E-clock -- the internal high-byte load edge and individual TOD
nibble carry phases -- are deliberately collapsed into the enclosing register
access or TOD tick. They require a per-phase CIA scheduler before software can
observe them; adding ad-hoc instruction delays would make their phase depend
on host slice size and is therefore outside the deterministic timing model.

The 68000 `RESET` instruction asserts the external reset line without
resetting the CPU core or clearing RAM. Copperline resets the CIA port
state on that line, so CIA-A releases `/OVL` and the boot ROM overlay is
visible again before Kickstart reads the reset vectors.

## Floppy (`floppy.rs`)

The floppy subsystem is track-timed: a drive has a rotational position,
and data under the head right now is what disk DMA sees. Track stepping
pays settle time, direction reversals cost more, and the index pulse fires
once per revolution into CIA-B FLAG. The stepper also enforces a minimum
step-pulse spacing (~40 us, 140 colour clocks): a pulse arriving sooner
after the last accepted one -- in either direction -- is ignored, so the
mechanism never over-steps on pulses faster than the head can move. Reads assemble MFM bitstreams from
the 11-sector AmigaDOS track layout; DSKSYNC matching, word-at-a-time
DSKDAT, and DMA into chip RAM behave as Paula documents. Non-WORDSYNC read
DMA drains Paula's recovered 16-bit disk word phase even when DSKLEN is
armed between disk-word boundaries; WORDSYNC is the explicit mode that
realigns framing to a matched sync word before transfer and again on every
later DSKSYNC match during it, so the sectors after an index wrap on a track
whose cell count is not a multiple of 16 still land word-aligned (AROS's
trackdisk.device reads 1.08 revolutions this way and scans the buffer on the
word grid; Kickstart's reads without WORDSYNC and bit-searches itself).
DSKBYTR is driven by the track read shifter: received bytes and `DSKBYT`
reflect the shifter's bit counter (reset by `WORDSYNC` matches whether or not
a DMA transfer is active), while `WORDEQUAL` reflects the instantaneous comparator
level (true only for the single cell matching `DSKSYNC`). Reading `DSKBYTR` clears
`DSKBYT`, while the `DSKSYNC` interrupt latches the comparator edge. This ensures
byte framing from arbitrary sync word phases on IPF and flux tracks, which is
required by protection schemes such as Copylock (e.g. *Lemmings*).
Supported image
formats: ADF (read/write), gzip ADZ, single file ZIP, DMS (decompressed by
 `dms.rs`), UAE extended ADF, and read-only IPF (decoded by `ipf.rs`) and SCP
images.
Connected mechanisms with no media keep the active-low disk-change line
asserted; a step pulse only clears that latch once media is actually
present, so guest software sees a no-disk condition rather than unreadable
track data.

Standard ADF and AmigaDOS tracks are synthesized as one PAL-sized
revolution: 11 sectors occupy 5984 MFM words, and the generated revolution
is 6334 16-bit MFM words so the index gap matches normal Amiga floppy
timing. This matters for raw loaders that DMA a fixed-size window and make
their own assumptions about the post-sector gap. UAE extended raw tracks,
IPF tracks, and SCP flux captures keep their stored track length and
per-revolution timing instead of using this synthetic geometry.

IPF images are decoded by `ipf.rs` rather than the closed-source `capsimg`
library. The format stores each track as blocks of *stream samples*: sync
marks and raw runs are already-encoded MFM written through untouched (which
is how an address mark keeps its illegal clocking), while data and gap
samples hold decoded bytes the loader MFM-encodes, setting the clock bit
only between two zero data bits. Block gaps are filled either from a single
repeated byte or from forward and backward gap streams whose loop samples
stretch to meet at the write splice in the middle. Each track is checked
against the bit counts its descriptors declare and then rotated so the
revolution starts at the index, matching the shape a flux capture already
has. IPF track density variations (`IMGE` density field) are modeled as cell-time
weights relative to nominal speed according to CAPS library specifications:
Copylock Amiga masters key sectors at -5.5%, -0.5%, and +4.5% (blocks 4-6 or
0-2), while Copylock ST, Speedlock, and Brierley models apply +/-5-15%
weighting per block. Scaling the cell clock (`TrackRev::prefix_cck`) allows
timing loops polling `DSKBYTR` to detect intentional cell density differences
(such as Copylock checks on *Lemmings*). Weak bits currently replay as the
single deterministic revolution stored in the file.

The synthesized drive sounds ([](../guide/configuration)) are driven by
this model's real state transitions -- motor spin-up, seeks, the
empty-drive poll click.

Floppy drive motors are controlled by an internal latch: `/MTR` is sampled on
the falling edge of `/SEL`, and the level of `MTR` while selected is ignored.
Starting the motor requires asserting `/SEL` while `/MTR` is low; stopping
requires `/MTR` to be high before and after the select transition (matching
WinUAE and vAmiga hardware tests). External drives clock their drive ID shift
registers off these select edges while the motor is stopped; `DF0:` contains
no ID hardware.

Disk DMA against a mechanism that cannot deliver cells -- no media in the
drive, or the motor line off -- arms normally and then pends: Paula has no
readiness interlock and waits for data forever, so the guest's own timeout
governs. A media insert or motor start mid-transfer brings the pending
transfer to life exactly as on hardware; nothing completes early (the
turbo burst also refuses drives that are not ready).

## Known AGA/ECS gaps and non-goals

Most ECS and AGA behaviour is implemented (the register notes above and
[](video.md)); the chipset gaps that remain are:

- **Sub-unit AGA DDF stop effects** beyond whole-unit completion are not
  modelled; the current model starts from DDFSTRT and rounds DDFSTOP
  through complete FMODE units.
- AGA palette reads through BPLCON2.RDRAM are modelled, including BANK/LOCT
  selection and the read-only COLORxx window. Other ECS register readback is
  pinned by unit tests and the vAmigaTS sweep.

Deliberate non-goals, recorded so they are not re-investigated: A2024 /
UHRES dual-scan display (a one-time "not emulated" warning is kept),
genlock ZD output beyond register storage, and AGA "double CAS"
memory-timing fidelity beyond what `timing-test/` measurements justify.
