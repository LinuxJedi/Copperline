# The video pipeline

The renderer's central rule: **it never races the chipset.** The chipset
does not paint pixels as it runs; instead, every render-relevant event is
recorded with its beam position, and the renderer replays the completed
frame's events afterwards. The live emulation and the painting of pixels
are decoupled in time but exact in beam position. In normal windowed and
headless runs, replay happens on the default render worker; the CPU,
custom-chip model, and GPU presentation remain on the main thread.

## Recording: beam events (`video/beam.rs`)

As the core runs, Copper and CPU writes to render-relevant registers --
BPLxPT, BPLCONx, COLORxx, DIWSTRT/STOP, DDFSTRT/STOP, modulos, sprite
registers -- are recorded as `BeamRegisterWrite` events tagged with
`(vpos, hpos, source)`. Chip-RAM writes that can affect a frame already
being fetched are recorded similarly. `BeamEventIndex` buckets events per
scanline so replay does not rescan the full frame log per line.

## Replay: planar to RGBA (`video/bitplane.rs`)

At frame end the renderer starts from a snapshot of display state, then
walks each scanline applying that line's recorded events at their beam
positions: a palette write at `hpos` changes the colour of pixels to its
right, a mid-line BPLCON1 write shifts scroll mid-line, exactly as the
beam would have seen it. Bitplane data is fetched via the recorded BPLxPT
state in the hardware fetch order, shifted through beam-timed BPLCON1,
decoded through EHB / HAM / HAM8 / dual-playfield rules (the pixel
pipeline carries 24-bit colour end to end; OCS/ECS paths keep their exact
12-bit maths and expand by nibble), composited with the eight sprites
under playfield priority, and CLXDAT collisions are accumulated.
The CLXCON/CLXCON2 playfield classification is a frame-local 256-entry
truth table retained across scanlines and rebuilt when its control key
changes. Each framebuffer collision entry is packed into one byte; this
is only a representation change, and the same playfield-presence and
match bits feed sprite priority and CLXDAT.
For DMA-fetched HAM playfields, the display window gates framebuffer output
and collision recording, but it does not rewind the HAM component history:
Denise's hold register advances on every shifted sample, so fetched samples
that sit before DIW opens (a late DIWSTRT, or an early DDFSTRT) still
advance the hold colour, and replay pre-advances those hidden samples
before painting the DIW edge. The standard `$81` window edge
is flush with the standard `$38` picture (both at framebuffer x 62,
hardware-verified on the sblit0 A500 photo), so a stock screen hides no
samples. Overscan HAM pictures rely on the hidden span: the Lemmings 2
FES demo's DMA Design logo (DDFSTRT `$30`, DIW HSTART `$79`) opens each
line with a set-palette pixel in the eight hidden lo-res samples, and
bounding the history to the display-phase samples turned its left edge
into modify-green streaks. Single-word lo-res fetch placement is linear in DDFSTRT: each 8-cck fetch
period before the standard `$38` slot moves the picture exactly 16 lo-res
pixels left (hardware-verified
against the vAmigaTS `Agnus/DIW/OLDDIW/diw1` A500 photos, OCS and ECS).
Early and late single-word lo-res DDF keep the picture beam-anchored;
the renderer must not add or subtract a sample just to
align the picture to a fetch-unit boundary.
Hi-res early DDF is beam-anchored the same way: content fetched ahead of the
window edge is hidden by the window comparator alone (XSysInfo's DDFSTRT `$38`
panel clips exactly its one pre-fetch word), so when an extreme-overscan
screen opens the window early as well (KS 3.2 Overscan editor on ECS:
DDFSTRT `$28` with DIWSTRT h `$5D`), the early words are visible inside the
window rather than being snapped away (issue #186).
When DDFSTRT is late enough that DIW opens before DMA has delivered the
first BPL1DAT word for the row, playfield output remains border-colour until
that plane-0 fetch reaches Denise instead of sampling stale shifter contents.
That gate is placed in the bitplane/DIW coordinate domain, not the normal
Copper/register-write output domain, because it follows the fetch slot that
loads BPL1DAT.
Horizontal DIW clipping applies to sprites unless AGA border sprites are
enabled by BPLCON3.BRDSPRT; if BPLCON3.BRDRBLNK is asserted, the border-sprite
bypass is suppressed along with the blanked border.
Once that first DMA word is visible, the renderer samples the enabled
bitplanes from the complete latched word; it does not expose the first word
plane-by-plane according to each plane's individual DMA slot.
If a manual BPL1DAT write starts a word before a later DMA BPL1DAT load
point, replay stops the manual word where that DMA word replaces Denise's
shifter.
A manual BPLxDAT write (Copper or CPU, typically with bitplane DMA off --
the "chunky copper" display technique) loads Denise's holding register, and
the serialiser parallel-loads the held word on its free-running word
cadence, not at the write position: the 16-pixel batch snaps to the next
word-grid slot after the write's bus landing (slots every 32 framebuffer
pixels in lo-res and 16 in hi-res, anchored two pixels left of the DIW
`$81` column) and is DIW-clipped there like any fetched pixel. Writes four
colour clocks apart can land in the same slot, and re-arming before the
load strobe replaces the held word instead of starting a second batch, so
a per-line raced (COLORxx, BPL1DAT) stream renders as a continuous field
with a straight window-edge clip. Pinned by the `bplprobe-dat` golden
probe (WAIT-position sweeps against the DIW border plus double-write,
bit-order, scroll, and hi-res bands; vAmiga-verified byte-identical) --
the Desire "Hamazing" Hexagon left-edge regression class.
The OCS/ECS BPLCON1 scroll nibbles count lo-res pixels regardless of
resolution: one step shifts a hi-res playfield two hi-res samples and a
super-hi-res playfield four, and the comparison narrows with the word
cadence, so hi-res ignores nibble bit 3 and super-hi-res bits 2-3 (pinned
by the `ddfprobe-hscroll` golden probe on the Kickstart 2.05 boot-screen
constellation, vAmiga-verified). AGA's extended BPLCON1 fields feed the
same per-plane delays through `aga_bplcon1_scroll_samples`, masked to one
fetch-unit width (32-bit fetches scroll within 32 lo-res px, 64-bit within
64).
An off-grid DDFSTRT interacts with the scroll in both fetch regimes. An
FMODE=0 fetch placed off the shifter reload grid rounds UP (the data is
late for its own slot), and a scroll that covers the lateness catches the
floor slot one gulp earlier (vAmiga-verified, `ddfprobe-phase`). On a wide
FMODE fetch, Agnus masks DDFSTRT DOWN to the fetch-unit grid, so the data
arrives `earliness` px early relative to the programmed start, and
Denise's reload comparator runs on the absolute hpos gulp grid (it never
sees where the fetch started), so the fold boundary is the data-arrival
distance past the grid point: `earliness + pipeline`, with an 8-cck
fetch-to-comparator pipeline. Scroll taps at or past the boundary see the
next gulp's data and sit one full gulp left of taps below it. The
boundary saturates rather than wrapping at the gulp: arrivals slide
monotonically later as the phase grows, so once the boundary passes the
top of the tap range nothing folds, and an exactly on-grid start folds
from the pipeline alone (taps at or past 16 lo-res px).
Pinned by two golden probes, both FS-UAE-verified band by band and
re-verified against vAmiga 5.0b1's `A1200_2MB` AGA setup:
`ddfprobe-agafold` on the
Alien Breed II AGA playfield constellation (issue #248: lo-res BPL32,
DDFSTRT `$24` -> earliness 8 px, boundary 24), whose scroller pairs the
folded taps with a one-gulp pointer step and jumps 32 px for 4 of every
16 pan frames without the fold, and `ddfprobe-agafold2`, which sweeps
the DDFSTRT phase on the 64-bit fetch across the SANITY Roots II AGA
swirl/kaleidoscope constellation (issue #371: lo-res BPL64, DDFSTRT
`$58`/`$38` -> earliness 48 px, boundary past the 0..63 tap range), whose
taps 16..43 must render linearly -- the earlier last-earliness-window
rule (`fold at gulp - earliness`) reproduced AB2's map but folded every
Roots tap >= 16, pulling the swirl a gulp left and shearing the
kaleidoscope line by line. The hi-res/SHRES scaling of the pipeline is
not yet externally verified; only lo-res is pinned.
BPLCON1-delayed samples at the left edge of a scanline do not reuse the
previous line's final bitplane word. Before the current line's shifter has a
sample for a delayed tap, replay marks playfield output active but returns
colour index 0. Same-line samples fetched before DIW opened do scroll into
view, on every line alike: the delay taps pixels the shifter loaded from the
line's own pre-window fetch, so the first line of a bitplane-DMA block --
whether gated by a copper DMACON BPLEN write or by DIWSTRT's vertical
comparator -- renders its scrolled-in left edge exactly like the interior
lines (`ddfprobe-blockscroll`, vAmiga-verified; the Super Skidmarks CD32
menu corner-notch regression class). The scroll-in still never comes from a
previous scanline's tail. AGA's
extended BPLCON1 delays can exceed one 16-bit shifter word; the extra leading
gap also stays background until current-line samples reach Lisa.
A BPLCON1 write whose normal register position is already at or beyond DIW's
right edge is not pulled left into the current line's bitplane-scroll domain;
it updates following lines without retapping the visible HAM tail of the
current line.

The playfield pixel loop runs in control-run chunks: recorded control,
scroll, and palette events take effect at output-pixel boundaries, so
between two event positions everything derived from `ControlState` (the
BPLCON0 mode decode, display-window edges, fetch-origin quantization,
per-plane scroll delays) is constant and is computed once per run rather
than per pixel. The per-pixel decisions inside a run are unchanged -- the
chunking is a host-CPU optimisation, not a model change.
History-independent colour modes also cache their complete 256-entry
Denise/Lisa index tables in up to eight frame-local entries. The cache
compares the palette, the AGA colour-path selection and
the complete BPLCON0, BPLCON2, BPLCON3 and BPLCON4 registers. Scroll, fetch,
DMA, display-window and collision changes reuse that table: they affect
which sample is drawn or how its output is composed, rather than the
colour, colour latch and playfield mask resolved for a given sample index.
HAM -- HAM6 and Lisa's HAM8 alike -- remains on the sequential path because
every output depends on the preceding colour. An indexed lookup still
seeds the held colour for a later switch into HAM. Prepared planar rows
similarly share a single byte lookup when the odd and even BPLCON1 taps
have the same delay; the exhaustive prepared-pixel/word-sampler comparison
covers both that common path and
separate dual-playfield taps.

The horizontal display-window flip-flop is still the same 9-bit Denise
counter model. Lines without a mid-line DIW write solve its exact comparator
transition ticks directly; a changed line replays all 454 ticks. A randomized
equivalence test compares both paths across counter starts, wrap behaviour,
window bounds and carried flip-flop state.

The flip-flop's carried state also decides where the playfield painter
starts. A row that enters the framebuffer with the flip-flop open --
carried from the previous line, typically held by a DIWSTOP beyond the
counter's $1C7 wrap that never matches -- is not gated on the left by the
DIWSTRT position at all: setting an already-set flip-flop is a no-op on
hardware, and data fetched left of the DIWSTRT position is visible at its
fetch-derived beam position (vAmigaTS Agnus/DIW/OLDDIW diw10 A500 photos
prove the carried flip-flop; `timing-test/ddfprobe-diw1` is the golden
render probe). Chambers of Shaolin's Grandslam intro relies on this:
DIWSTRT H $C0 with DIWSTOP H $1D8 (unreachable) leaves the flip-flop
permanently open, and the logo fetched from the standard DDF $38 window
shows in full left of the $C0 anchor. The rule follows the carried-in
state, not the comparator alone: a row that enters open, closes at a
reachable HSTOP left of HSTART, and reopens at the anchor still reveals
the carried-in run's data, with only the closed gap masked as border.
Rows that enter the framebuffer closed keep the DIWSTRT anchor as the
paint start, so ordinary windows clip exactly as before. (Sprites still
clip by the register-derived window rather than the flip-flop -- a
remaining gap.)

BPLCON0 is itself split across two of those timelines. The plane count and
the resolution bits gate the fetch/serialiser side and stay in the generic
register domain, but the HAM select does not reach the shifter at all: it
picks how the already-serialised index becomes a colour, in the same
colour-selection phase a COLORxx write feeds. Replay therefore samples the
HAM bit `DENISE_HAM_SELECT_PIPELINE_FB` framebuffer pixels left of the rest
of the control state, so a HAM change and a COLORxx write carried by the same
chip-bus slot land on the same pixel (see `docs/internals/timing.md`; vAmiga
records the same relation in `Denise::setBPLCON0`). A game that paints a HAM
picture and an ordinary indexed panel on the same scanlines -- Hollywood
Poker Pro clears HAM at `WAIT hp=$A2` for its scoreboard -- otherwise decodes
the first columns of the panel as HAM modify commands. As with the
bitplane-scroll domain, a segment whose position has saturated past the
display window keeps the register-domain position. `hamprobe-select` in
timing-test/ pins the landing column against vAmiga.

AGA Lisa has one known split control path in this replay: BPLCON4's
high-byte BPLAM bitplane XOR follows the normal control timeline, but the
low-byte ESPRM/OSPRM sprite palette-base fields are visible to sprite
colour lookup at Lisa's earlier sprite palette-control x position. Ordinary
COLORxx palette writes stay on the Denise palette-output timeline; sharing
the sprite path shifts copper palette gradients horizontally and
turns smooth per-line colour ramps into bands.
The render event journal therefore creates a sprite-only BPLCON4 segment
when those two x positions differ, then applies the full BPLCON4 value on
the normal control segment.

Manual and held-sprite replay has a smaller split of its own. SPRxDATA and
SPRxDATB writes update Denise's data latches in the normal register-output
domain, but the sprite serializer copies those latches only when the
horizontal comparator fires. A DATA/DATB write after that compare is for a
later compare or scanline, not the word already shifting. SPRxPOS writes
re-arm the sprite horizontal comparator: if the write occurs before the
newly programmed HSTART, the sprite can still begin at that HSTART. The
replay clips those position intervals in the sprite-comparator domain
(seven CCK ahead of the normal register-output position) so adjacent manual
sprite words can abut at their HSTARTs and staggered even/odd attached-pair
position writes do not create artificial half-pair strips. Once a manual
sprite word has started shifting, later same-line POS/CTL writes can arm a
future compare but do not truncate that active word. A POS write that lands
exactly on the HSTART compare boundary is on the already-started side of
that rule.

When sprite DMA was observed for the frame, captured DMA lines are the
authoritative data source for DMA-fetched spans. Manual replay is seeded by
beam-timed SPRx register writes, not by frame-start SPRxDATA latches alone:
the data latch can persist across frames without proving that the sprite
vertical comparators are active in the current field. A same-line SPRxPOS
write after the sprite DMA slot can re-arm the horizontal comparator and
reuse the line data DMA already loaded, so the renderer seeds those POS-only
reuse spans from the captured DMA line. Sprites whose data was established
by DMA before SPREN was cleared are carried separately as held sprites and
can still be repositioned by later SPRxPOS/CTL writes. Merely enabling
sprite DMA and crossing an empty sprite pair slot is not enough to make
captured DMA authoritative; the frame must contain actual fetched or held
sprite data.

Sprite DMA starts at the top of the field above the active display window
(PAL line $19, NTSC $14). The pre-display scanlines are reconstructed by replay
in `frame_capture.rs`. This replay is paced cycle-by-cycle with the raster beam --
executing sprite slots as the beam advances and applying intermediate `DMACON`,
`SPRxPT`, and `SPRxPOS`/`SPRxCTL` writes -- because DMA reads dynamic Chip RAM.
Replaying pre-display cycles at display start previously allowed descriptors
modified by vertical blank interrupts to corrupt earlier sprite control fetches
(*Spooky Town* regression).

A DMA fetch arms the channel as it lands, but the serializer still only
copies the latches when the horizontal comparator fires, so a SPRxCTL write
between the fetch slot and HSTART cancels the fetched line outright: it is
displayed neither at its own HSTART nor at a position a later same-line
SPRxPOS write moves it to, because POS never re-arms. Captured lines whose
channel is disarmed before their comparator fires are dropped before
rendering and collision accumulation; a CTL write past HSTART leaves the
line alone, since it cannot recall pixels already shifted out. This is how a
Copper-multiplexed sprite panel retires its channels on the line below the
panel while sprite DMA is still fetching against a descriptor whose vertical
stop never matches -- Hybris's SCORE/LIVES/HIGH panel does exactly that, and
without the disarm the still-fetching channels paint a stray 16-pixel dash
under the digits (issue #278). `sprprobe-disarm` in timing-test/ pins both
directions against vAmiga.

Two manual-replay guards exist only to reconcile DMA writes the beam replay
cannot see (Agnus drives POS/CTL/DATA through the same Denise registers
without recording beam events): an early same-line SPRxPOS write hands the
line to the DMA capture, and a pre-visible SPRxDATA/DATB write seeds the
latch for later retiming instead of arming direct output. Both apply only
when sprite DMA was observed in the frame. With sprite DMA idle Denise's own
rules hold unmodified: SPRxDATA arms at any beam position (including
vertical blank), SPRxCTL disarms, SPRxPOS never disarms, and an armed sprite
serializes at HSTART on every line because Denise has no vertical
comparator. A vblank arm sequence with VSTART equal to VSTOP therefore
displays full-height columns, which is how Gen-X draws the vertical
edge-masking line sprites of its shutter transitions.

Because DMA fetches land in the same SPRxPOS/CTL/DATA/DATB registers a
CPU/Copper write hits, Denise keeps two views of them: the CPU/Copper write
shadow (`sprpos`/`sprctl`/`sprdata`/`sprdatb`/`spr_armed`), which the
manual replay above and the live collision path are calibrated against, and
the hardware-true view (`spr_hw_*`), which additionally receives every
sprite DMA fetch -- a DATA fetch arms it, the vstop control fetch
(including the 0/0 list terminator) disarms it. The DMA-idle latched
redisplay seeds from the hardware-true view: software relies on the
terminator's CTL to silence a channel for good, so a later bare
SPRxDATA arm must redisplay the DMA-written words, not the last manual
pattern (Hamazing's scene switch writes SPRxDATA=$0000 after a DMA sprite
scene and expects invisible sprites; the stale write-shadow pattern would
paint full-height bars). Only the authoritative sprite-DMA pass for a line
writes the hardware view through: pre-display lines are computed twice, and
the pre-display replay at the display start owns them (`sprprobe-latch` in
timing-test/ pins the whole sequence).

The mapping from beam coordinates to framebuffer x is anchored by
constants that encode the hardware's fetch-to-display pipeline delays --
register writes, palette writes, and bitplane data each land at their own
documented offset, and the bitplane fetch reference differs between lo-res
and hi-res. The display-window comparator maps a DIWSTRT hstart H to
framebuffer x = 2H - 196 (hardware-verified against the sblit0 A500 photo).
A standard lo-res `$81`/`$38` picture is flush with that edge; a standard
hi-res `$81`/`$3C` picture starts its 640 fetched pixels one lo-res pixel
inside the window (matching vAmiga), with no wider leading border. Wide-FMODE DMA fetches start from the revision-masked
DDFSTRT comparator value and complete whole units, but the displayed shifter
origin is still quantized by the FMODE fetch gulp; the renderer keeps those
two effects separate. That absolute gulp grid remains linear below the
standard fetch slots rather than clamping at the `$18` hard start. In lo-res
BPL64, DDFSTRT `$18` / DDFSTOP `$B8` therefore puts the whole first 64-pixel
gulp left of a standard `$81` DIW and fills the window with the remaining five
gulps; `ddfprobe-agaorigin` pins the hidden first gulp and the flush right edge
against an equivalent FS-UAE A1200 capture. Denise's output line starts at the horizontal blanking
start counter; COLORxx writes before that counter are the wrapped tail of
the previous output row, while the palette value they load is still the
base colour for the following row. These anchors were calibrated against
real-hardware captures and other emulators; `COPPERLINE_HCENTER=0` and
`COPPERLINE_OVERSCAN=full` help when re-checking them.

For FMODE=0 lo-res, the one-sample low-res phase bias is applied on both
standard and late fetch origins. If a late DDF row completes exactly at
DIWSTOP, the final visible DIW sample still includes undelayed planes; BPLCON1
delay only retaps the per-plane shifters, it does not make the undelayed planes
drop one sample before the display window closes.

The framebuffer is a 716x285 overscan field (lo-res pixels doubled
horizontally). It captures deep overscan on all sides.
For standard 15 kHz PAL/NTSC fields, row zero is anchored at Copperline's
fixed overscan top rather than the current DIWSTRT vertical value. DIW still
acts as the hardware display-window flip-flop: it decides when the frame's
chip-RAM snapshot and bitplane DMA capture begin, but changing DIWSTRT later
in the field does not recenter the already-visible top border. Programmable
VARBEAMEN scans instead use their programmed visible window as the render
origin. Under VARBEAMEN, Denise's horizontal counter restarts at 0 with the
programmable line rather than free-running at the standard 15 kHz phase, so
the DIW and sprite comparators sit later on the canvas by that origin
difference (Linux/m68k amifb and the KS3.1 DblPAL screen both program their
windows against the zero origin). A programmable frame is presented like a
multisync monitor on both axes: when the mode programs its sync pulses, the
glass shows the line from the HSYNC trailing edge to the next pulse
(VARHSYEN) and the frame from the VSYNC trailing edge to the next pulse
(VARVSYEN), so the picture sits where the mode's own porches place it, with
blanked border rows above and below the programmed vertical window. Without
a programmed horizontal sync the whole line maps onto the glass
time-linearly (each colour clock covers 227/line_cck of a standard clock's
width); without a programmed vertical sync the captured rows keep covering
the full glass height.

Super-hi-res output: Denise/Lisa resolve every 35 ns sample through the
full palette pipeline (ECS Denise carries at most two bitplanes into
SHRES; AGA Lisa runs the complete 8-bit index path, e.g. the 4-plane
FMODE=3 Linux amifb console). A programmable scan that drives SHRES
renders a double-width canvas at the 35 ns pixel pitch
(`canvas_scale_for`): each of the two per-column samples is emitted as
its own framebuffer pixel, and the presentation, screenshots, and the
browser canvas carry the doubled width through (the desktop window shows
it 1:1 on a 2x HiDPI texture). Every logical coordinate in the replay --
comparators, fetch origins, sprite positions, the collision buffers --
stays in the classic hi-res-pitch domain; only the framebuffer writes
fan out. The sprite serializer keeps its own 35 ns sample coordinate:
BPLCON3 SPRES=11 emits one sample per doubled-canvas pixel, SPRES=10 emits
two, and the lo-res encodings emit four. Standard 15 kHz scans keep the
classic single-width canvas byte-identical; their SHRES playfield and
sprite subpixel pairs are blended into each 70 ns pixel. The renderer keeps
the two playfield colours and priority masks separate until sprite
composition, while CLXDAT keeps its 70 ns column pitch and combines adjacent
35 ns sprite samples. Sprite comparator positions remain at hi-res resolution
on either canvas, as on Lisa; SPRES changes pixel width, not the comparator's
positional granularity.

Two vertical edge cases the replay honours:

- A display window can open above the captured canvas. Bitplane pointers are
  pre-advanced for those clipped rows by replaying the frame's
  BPLCON0/DMACON writes line by line, so only lines where bitplane DMA
  was actually enabled consume a row -- the CDTV boot screen opens its
  window at line 5 but raises BPLCON0 from 0 to 6 planes at line 24.
- DIWSTRT=0 is not a sentinel. If DIWSTOP is non-zero, the replay opens the
  display window at beam zero and clips the overscan rows/pixels that fall
  before the captured framebuffer; only DIWSTRT=DIWSTOP=0 falls back to the
  reset/default visible window.
- Canvas rows whose beam line lies at or past the frame wrap (the fixed
  285-row field is taller than a standard PAL scan) are forced to black:
  the beam never produces those lines, and a deep-overscan window would
  otherwise let the replay keep walking bitplane memory past the image.

## Threaded frame handoff (`RenderInput`, `video/window.rs`)

At frame end, `Bus::begin_new_beam_frame` freezes the just-finished frame:
the render-event journal, chip-RAM snapshot, captured bitplane/sprite DMA
rows, palette split, display geometry, frame line count, framebuffer start
line, and Agnus programmable blanking latches become the source for
`RenderInput::from_bus`. The large immutable chip-RAM and captured-bitplane
bundles use shared ownership between the bus and `RenderInput`; queueing a
worker job therefore does not copy the full RAM image or deep-clone every
plane row. A completed job releases those references before the next frame
wrap so the RAM allocation normally returns to the capture side. Released
bitplane rows are kept in a bounded capture-side pool, preserving the eight
plane-vector allocations across frames while clearing their contents before
reuse.
`render_from_input` consumes only this frozen bundle, so the main thread can
start emulating frame N+1 while the worker renders frame N.

Each render thread also retains a `RenderScratch` arena across calls. The
per-row base palettes and control state, their nested segment vectors, the
playfield/collision canvases, horizontal-window rows, DMA-output origins and
manual-HAM selector buffer are cleared and resized in place. The merged
palette-event journals, sprite lines, attached-pair beam lists and full-frame
sprite collision mask live there too. This preserves their allocations across
changing frames and avoids rebuilding several large temporary buffers at
field rate; thread-local ownership keeps the synchronous browser/native paths
and the desktop worker independent without locking.

`window.rs` starts a persistent `copperline-render` worker by default.
`COPPERLINE_THREADED_RENDER=0` (also `false`, `off`, or `no`) disables the
worker and uses the synchronous wrapper path. The default worker owns a
scratch framebuffer and the deinterlacer history, calls
`bitplane::render_from_input`, applies the same presentation post-processing
as the synchronous path, and returns a presentation framebuffer tagged with
the render generation and emulated frame number. Resets, power changes, and
save-state loads bump the generation so stale worker results are ignored
instead of being shown after the machine timeline changes.

The worker never mutates emulator-visible hardware state. `CLXDAT`
collisions are CPU-visible Denise state, so the bus completes unread live
collision replay to the end of the frame before rolling the frame buffers.
The synchronous fallback still ORs the render result's collision bits into
Denise after painting, but the threaded path treats those bits as diagnostic
render output and records only the returned render timing on the main
thread.

wgpu and winit remain main-thread-only: the worker paints CPU buffers, and
the main thread uploads the newest completed presentation buffer to the
`pixels` surface. Normal display can be one frame behind emulation; exact
capture paths call `finish_render_for_current_frame` so screenshots, frame
dumps, recordings, debugger step, and run-to-PC output use the requested
emulated frame.

Run-ahead (`[emulation] run_ahead_frames`) sits above this pipeline. A burst
first retires one committed anchor frame, snapshots the machine, and then
retires `n` speculative frames with per-frame pacing suppressed. Every
speculative frame is silent: `AudioMux` drops all master/source/channel fanout,
and Paula withholds completed serial words from both its sink and observer.
The final future frame is synchronously rendered while its Bus is still live;
only then is the anchor restored. Rendering after restore would present the
past, and merely submitting a worker job before restore would race the
fallback renderer. One `pace_runahead_burst` call at the end uses the anchor's
emulated time, so snapshot, speculation, rendering, and restore all share one
display-period budget. Speculative frames contribute host busy time but not
the committed-frame counter.

The eligibility gate is deliberately conservative. It excludes any device or
observer with host state that `M68kMachine::write_state` cannot rewind; the
user guide lists the current set. A snapshot or restore failure disables
run-ahead for the session instead of silently advancing by extra frames. If
stepping or rendering interrupts a burst after speculation has started, the
anchor restore is still attempted before run-ahead is disabled: output from
those abandoned future frames was suppressed and they cannot become the
committed timeline.

Progressive frames have two exact reuse checks. First, two consecutive
renders with identical lightweight inputs arm a pre-render key containing
every captured bitplane row, sprite line/latch, register/event stream,
geometry and blanking input. If replay fetched words from the chip-RAM
snapshot, the key also retains each timed address/value dependency and
replays those reads against the next snapshot; unrelated guest RAM therefore
does not defeat reuse. A match skips replay and keeps the prior collision and
presentation result. The two-frame arming step avoids deep-copying captured
plane data on changing displays. Interlace, phosphor history, and
time-dependent render diagnostics do not take this shortcut.

The browser wrapper exposes a monotonically wrapping presentation revision.
It advances only after a non-reused frame has completed post-processing and
been copied into the page-facing presentation buffer. JavaScript remembers
the last uploaded revision, so an exact pre-render reuse also suppresses the
typed-array construction, texture upload and monitor draw. Display-only
changes such as a resize, tint, monitor mode or WebGL context restoration can
force a draw of the held texture without pretending the emulated picture
changed.

The WebAssembly frontend shares presentation mathematics with the desktop
client via `present_common`: autocrop hysteresis (`AutocropLatch`), per-axis
integer fitting (`per_axis_fit`), and coordinate mappings translating renderer
content boundaries to presentation buffer pixels (`FieldPlacement::content_rect`
and `region_present_content_rect`). Under integer scaling, 60 Hz NTSC modes
present native scanlines without vertical resampling to PAL row counts. WebGL
monitor passes draw the cropped sub-rectangle via the `u_src` uniform (matching
`crt.wgsl`'s `src_rect`), while 2D canvas fallback scales to the device
viewport.

For a progressive frame without phosphor persistence, presentation writes
directly into the frontend-owned buffer instead of first filling the
deinterlacer's full woven buffer. The browser's standard-TV path goes one step
further: it maps the captured aperture's destination rows straight back to
the source field, combining line doubling, PAL/NTSC vertical presentation
scaling and cropping in one copy. LACE fields and phosphor persistence keep
the history-dependent weave/blend path and then crop its output. The
deinterlacer grows its weave, motion-mask and phosphor buffers lazily, so a
frontend that disables both effects pays neither their frame cost nor their
multi-frame scratch allocation.

After post-processing and deinterlacing, the frontend compares the complete
active presentation buffer and its geometry with the current one. This is a
word-for-word comparison, not a hash. An exact repeat keeps the current
buffer and, when the status LEDs/media state and overlays are also unchanged,
does not request a main-window redraw; that avoids the CPU copy, texture
upload, and GPU present even when harmless differences in the raw event log
made the conservative pre-render key reject the frame. Recording still emits
the duplicate video frame, and status/UI changes still redraw over a held
Amiga picture.

## Interlace (`video/deinterlace.rs`)

Interlaced (LACE) content is presented through a motion-adaptive
deinterlacer at double height: each field lands on its parity's output
rows, and opposite-parity rows are filled by weaving the previous field
where content is static and interpolating neighbours where it moved.
Motion is detected on both parities (each field against the previous
field of its own parity, and the woven line against its own
predecessor), and the per-pixel motion mask is dilated one pixel
sideways so dithered moving art bobs as a region instead of weaving and
interpolating on alternate pixels.

Fields are routed by the hardware convention (the long field, LOF=1,
carries the upper output rows), but software that never observes LOF and
simply ping-pongs two per-field images - a chained pair of copper lists,
each repointing COP1LC at the other - lands on an arbitrary pairing
decided by which field happened to be running when its chain started. A
real CRT hides the wrong phase in field-rate flicker; a progressive weave
turns it into a one-line comb through every detail. The deinterlacer
therefore measures both pairings' cross seams against the stored opposite
field each laced push: motion widens both sums about equally and fails
the margin test, but a static picture drawn for the opposite pairing makes
the losing sum carry the comb, and after four consecutive losing fields
the weave phase flips (relabelling the stored field history with it). The
flip and its votes reset whenever the weave history is dropped -
progressive fields, machine swaps, resets, state loads, deinterlace
toggles and scan changes alike - so a stale phase from one stream never
routes the next stream's fields to the wrong parity. Kang Fu CD32's
laced HAM8 intro screens are the regression example: with the phase
wrong, its thin lettering rendered as "every other line missing".
Progressive content is line-doubled without history. With phosphor
persistence off, the common progressive path writes those doubled rows
directly into the frontend-owned presentation buffer instead of filling
the deinterlacer's intermediate output and then copying the complete
frame. Interlaced and phosphor-blended frames retain the history buffer.
`[display] deinterlace = false` (or the `COPPERLINE_DEINTERLACE=0` env
override) falls back to plain line doubling; like phosphor, the setting
travels in every render job.
The browser deliberately starts with both history-dependent effects off for
throughput and exposes live controls for pages that prefer their CRT
presentation. Desktop defaults are unchanged: motion-adaptive deinterlacing
remains on and phosphor persistence remains off.
In the default threaded pipeline the worker owns this history; the
synchronous fallback keeps it on the window `App`. The worker drops its
history whenever the render generation changes (machine swap, reset,
state load), so nothing from the previous presentation stream weaves or
glows into the next one.

The deinterlacer also hosts the optional CRT phosphor-persistence stage
(`[display] phosphor` / `COPPERLINE_PHOSPHOR`, off by default, clamped to
0.95): when on, `present_with_phosphor` blends each presented frame over a
retained copy of the previous one, keeping `phosphor`/256 of the old value
per channel for an exponential trail. This is what fuses field-rate flicker
(alternate-field dither transparency, flicker-dithered animation) the way a
real tube does. Like the rest of the deinterlacer it operates on the
presentation buffer only and never touches the emulated framebuffer. The
persistence fraction travels in every render job rather than being fixed at
worker spawn, so a machine started from the launcher applies its configured
value.

## Known display gaps

- **31 kHz horizontal layout** (DblPAL / DblNTSC / Productivity): at
  doubled scan rates the bitmap lands ~16 colour clocks left of the DIW
  window edge, and fetched data draws past the short line's end instead of
  being cut by the line wrap. Pinning the per-line DIW/fetch anchoring
  needs WinUAE / real-hardware reference captures; the image-regression
  suite covers these modes structurally but does not yet assert exact pixel
  positions.
- **Programmable interlaced (FF) weaving** is implemented but untested
  against real software.

## Presentation (`video/present_common.rs`, `video/window.rs`, `video/ui.rs`)

`window.rs` owns the winit `ApplicationHandler` and the `pixels` GPU
surface: the field is presented at a TV-like 4:3 aspect plus the
44-pixel status bar, scaling continuously with the window. The GPU surface
is fed from `present_fb`, the post-processed presentation buffer produced by
either the render worker or the synchronous fallback.

The emulator window is drawn onto the surface by its own scaling pass
(`window/scaler.rs`), not the `pixels` crate default renderer (tool windows
retain the built-in Fill renderer). The custom scaler pass accepts destination
rectangles and filter modes directly, allowing integer scaling multipliers beyond
4x on high-resolution displays. Point sampling remains exact because the present
copy replicates each canvas pixel into a uniform texel block. Smooth filtering
uses a sharp bilinear shader with texel snapping. `PresentLayout`
(`window/present.rs`) is the single source of truth for display geometry, cursor
coordinates, and overlay positioning.

The scaler draws an opaque black background across the surface before drawing
the picture and chrome. A render-pass clear alone can leave corrupt colour and
alpha values in the letterbox bars on Intel Mac Metal presentation surfaces,
even when the same clear works on an offscreen texture. The background draw
also covers frames with no display rectangles.

`[display] autocrop` samples the active content sub-rectangle of the canvas each
frame (`RenderResult::content_rect`), computed from raster lines containing
fetched bitplane data. Chrome elements (panels and status bar) are drawn
separately in a docked bottom band sized identically to the letterboxed layout.
`AutocropLatch` filters frame-to-frame variations: viewport expansions apply
immediately, while reductions require stability over a debounce window to prevent
zoom pumping during transitions. Opening menus or panels expands the display
quad to the full display area so overlays remain visible. CRT presets render over
the cropped area. Programmable multisync modes (e.g. DblPAL) are cropped using
their sync-anchored windows. Autocrop is suspended when using monitor bezels or
RTG modes; capture outputs retain full aperture.

Integer scaling presents from the unresampled canvas in both TV and square pixel
modes (`video::square_canvas`). In TV mode, `per_axis_fit` calculates horizontal
and vertical integer multipliers independently to match the 4:3 CRT pixel aspect
ratio (`glass_par`). On a 1080p display, 200-line NTSC content scales to a 4:5
pixel aspect (1280x1000). Monitor bezels retain the resampled TV canvas, while
RTG and programmable modes scale uniformly.

Every redraw first re-syncs the surface to the host window's current size
(`resync_surface_size`), rather than trusting the Resized event to have
arrived first. `pixels` rebuilds its swapchain from the size the last
`resize_surface` gave it, and retries the acquire in a loop with no bound,
so a surface left behind by a resize the app has not seen yet is a hang and
not a misdraw: a driver that rejects the mismatched extent (Mesa's X11
Vulkan WSI answers `VK_ERROR_OUT_OF_DATE_KHR`) sends that loop round
forever, and because it runs inside the event callback it also starves the
Resized event that would have corrected the size. Entering or leaving
fullscreen is the common way in, the window manager resizing the window a
moment before the event is delivered. Both window kinds record the size
their surface was configured with, and all resizes go through the wrappers
that keep that record in step.

The frontend-independent half of this pass lives in
`video/present_common.rs`: the post-render pipeline (vertical/horizontal
recentring, the TV bezel mask, programmable-scan presentation) plus the
standard-window and TV-aperture constants and the geometry predicates that
key on them. `window/present.rs` re-exports everything there, so the
desktop path is unchanged; headless consumers -- `cpu.rs`'s debug
screenshots and the [browser (WebAssembly) frontend](../guide/browser.md)
-- present frames through it without the winit stack.

Two presentation-only adjustments (they never alter the emulated
framebuffer):

- **Overscan mask**: `[display] overscan = "tv"` masks deep-overscan
  margins in black like a CRT bezel; `"full"` shows the entire field. The
  default TV mask is presentation-only: horizontally it keeps 24 lo-res
  pixels of consumer-visible overscan beside the standard display and blacks
  only the deeper horizontal margins. TV mode keeps the framebuffer's fixed
  horizontal source origin, matching the way vAmiga and FS-UAE crop from their
  rendered source texture instead of copying the picture sideways. Vertical
  border colour changes remain visible because they are part of the Denise
  output and are often deliberate border effects.
- **TV glass**: normal screenshots and `--dump-frames` in TV mode present the
  same 716x540 4:3 glass as the live window. Horizontally the framebuffer's
  captured aperture (`TV_CAPTURED_*`, 668 columns) contains the 640-pixel
  standard display with 14 captured overscan pixels on each side. Those real
  columns are nearest-neighbour resampled across the 716-pixel glass, so the
  raster reaches both edges without synthetic black bezel columns. Vertically
  the aperture follows the scan the frame actually ran: a 312/313-line (50 Hz)
  field contributes 540 woven rows, while a 262/263-line (60 Hz) field
  contributes 428. The shorter crop is resampled onto the same 540 output rows,
  keeping PAL and NTSC presentation the same shape. True horizontal overscan
  fetches are not cropped to this aperture: they stay on the full-width path
  so intentional border content remains visible. `COPPERLINE_SHOT_RAW=1`
  bypasses presentation and writes the raw 716x570 woven framebuffer. The
  captured-aperture geometry invariants are const-evaluated beside the
  definitions. While a monitor bezel is drawn, the live window widens this
  vertical crop to the *tube aperture* (`TUBE_*_PRESENT_HEIGHT`): the whole
  rendered field -- 570 woven rows on a 50 Hz scan, 470 on a 60 Hz one,
  from woven row 0 -- resampled onto the same glass, because a real 1084's
  visible raster exceeds even the whole captured field. The widening is a
  live-window decision keyed to the bezel style alone (not to the bezel
  *pass*, which an open overlay suspends -- the picture must not jump
  between apertures when a panel opens); captures keep the TV aperture, so
  screenshots stay byte-identical with headless runs whatever front is
  drawn.
- **Full-overscan horizontal recentring**: in `"full"` presentation, a standard
  (non-overscan) display is recentred because the framebuffer captures a deep
  slab of left overscan that would otherwise push the picture right of centre.
  The decision keys off the bitplane data the display actually fetches (DDF),
  not just the DIW window: a demo that opens DIW wide around a standard-width
  picture (Virtual Dreams' "Absolute Inebriation") is still recentred, while a
  display that genuinely fetches bitplane data into the overscan border is left
  exactly as rendered.

Both content-keyed decisions -- the TV aperture crop and the full-overscan
recentring shift -- are latched across border-only frames
(`PresentationLatch` in `present_common.rs`). A frame with no bitplane
content intersecting the window (registers cleared during boot, or the
blank frame or two Intuition emits at every screen change while it rebuilds
the copper list) carries no evidence about the display's layout, so it
keeps the previous frame's geometry instead of snapping to the full
framebuffer -- the monitor does not move between screens. The power-on
default is the stock standard display (aperture on, standard recentring
shift); a frame that does carry content, including a true-overscan fetch or
a programmable scan, re-latches the decision, so an overscan demo that
blanks between parts stays on full-frame presentation throughout. The latch
resets on presentation discontinuities (machine swap, reset, state load).
Frame dumps and screenshots share the resolved decision, so a TV-mode dump's
PNG dimensions remain 716x540 across a boot.

### RTG scanout (Z3660 and Picasso II/II+)

When a fitted `[rtg]` board's guest driver switches the display to RTG,
the presentation path swaps sources: the board's panned framebuffer
(decoded from VRAM in the scanout's pixel format, with its hardware cursor
composited over it) replaces the chipset render. Z3660 implements its FPGA
scanout and sprite in `z3660.rs`; Picasso II/II+ implement the CL-GD5426/5428
scanout, two-plane cursor, and physical pass-through switch in
`picasso2/gd5426.rs`.
The window presents
that frame at its native resolution through a dedicated GPU texture
rather than the 716-wide chipset buffer, and the TV aperture crop is
suppressed -- it is a chipset crop rect, and applying it would show a
sub-rect of the board's screen. While a menu or panel is open the window
falls back to the CPU present path (at the cost of the downscale) so the
overlay is not overdrawn by the GPU pass. If the board claims the display
but its frame does not compose yet (mode set before the resolution
registers), presentation falls back to the chipset render rather than
freezing on a stale frame.

`compose_rtg_present` (`present_common.rs`) also keeps an
`FB_WIDTH`-stride copy of the native frame for the screenshot and CCP
capture paths, which read the shared presentation buffer: one output row
per board row at the board's native height, downsampled horizontally by
sampling each output pixel's source-span centre so the rightmost source
columns survive. Screenshots under RTG are therefore 716 wide at the
board's native row count.

Picasso II and II+ remain on native pass-through after reset. Even after the guest
writes its VGA-output switch, `rtg_active` requires a running, unblanked
sequencer and a plausible CRTC mode whose visible rows fit in VRAM. During
driver mode changes this makes presentation fall back to the native chipset
frame instead of exposing stale or out-of-bounds VRAM.

`ui.rs` implements the status bar widgets, the pop-up menu, the smaller
overlay panels (About, Shortcuts, Calibration), and the shared debugger/tool
panel drawing used by the native debugger and frame-analyzer windows. The UI
uses the 8x8 `font.rs` glyphs. `COPPERLINE_UI_PREVIEW=1 cargo test
panels_render_into_their_rects` renders every panel into
`target/ui-preview-*.png` -- the screenshots in this documentation come
from there -- and the `test_app()` fixture drives the debugger window
against a real emulator instance in the unit tests.

The configuration panel lives in `ui/configuration.rs`, with row drawing and
hit-testing together in `configuration/rows.rs`, library artwork and entries in
`configuration/library.rs`, and modal drawing and hit geometry in
`configuration/dialogs.rs`. Its editable model remains in `launcher.rs`.
`launcher/fields.rs` defines tabs, rows and choices; `launcher/values.rs` owns
labels and edits, sharing a single mapping for ordinary boolean fields.
Boot-priority arrows use the shared boot-field-to-drive mapping, so every
listed drive follows the same stepping and Bootable rules.
Hardware-dependent edits remain explicit. `launcher/setup_config.rs` handles
loading and serialization, with `to_raw` delegating to machine, media, WHDLoad,
presentation, I/O, audio and expansion sections.

### CRT shader pass (`window/crt_shader.rs`)

The optional tube emulation (`[display] shader`, off by default) is a second
pass inside the same `pixels` `render_with` closure the RTG texture uses:
the presentation scaler pass draws the composited buffer first, then
`CrtShader` re-draws the display rectangle through a fragment shader. Its viewport is
the display sub-rect of the letterboxed clip rect -- the clip rect scaled by
`present_height() / window_present_height()`, the same multiply-then-divide
the RTG display rect uses so the two land identically -- and it samples only
the matching `src_rect` of the presentation texture, so the status bar
below is neither read nor overdrawn. `uniforms_for` builds the uniform block
and that viewport from pure arithmetic, with no GPU state, so the mapping is
unit tested on its own.

One 64-byte uniform block goes to the GPU per presented frame: the display
sub-rect in UV, the viewport size in physical pixels and the source region
in texels, the strength, the scanline count, and the per-preset mask,
curvature and vignette knobs. Three presets (`shaders/scanlines.wgsl`,
`mask.wgsl`, `crt.wgsl`) are `include_str!`-embedded and compiled into
pipelines at window creation, so switching preset at runtime is a pipeline
selection, not a compile.

`sample_display` clamps the sample half a texel inside `src_rect` rather
than to its edge. On the boundary a linear tap is a 50/50 blend with the
texel on the far side, which along the bottom edge is the status bar's
separator hairline; that reaches the picture whenever the display rect is
magnified (the last fragment row lands past the last texel centre).

The `crt` preset's curvature bows only the face outline, never the
picture: a real monitor's deflection is corrected so the raster is
rectilinear on the curved glass, so the picture (and its scanlines) is
sampled straight and the warped coordinate feeds only the face's signed
distance. What lies outside the bowed outline is the unlit inside of the
tube, opaque black at any strength, and only the *area* of that region
scales with strength (through the warped coordinate, so strength 0 has no
off-face region at all and the no-op invariant holds). Mixing the black
back toward the sample instead would leave the region holding a fraction
of the edge colour the clamp smears there. The boundary is faded to black
over about one pixel, keyed to `fwidth` of the signed distance to the
face, so the curve does not staircase.

The scanline count is what the window actually shows, not what the
framebuffer holds (`crt_scanline_count`). The TV-aperture present path
copies the standard scan's aperture crop (`TV_PAL_PRESENT_HEIGHT`, 540
rows, or `TV_NTSC_PRESENT_HEIGHT`, 428) rather than the whole woven
buffer, so its count comes from the aperture -- 270 lines on a 50 Hz scan
and 214 on a 60 Hz one, against 285 for a standard field in `"full"`
overscan, or the tube aperture's 285/235 while a bezel widens the copy --
and is rescaled by the rect/content ratio when the
square-pixel canvas pads the aperture with bezel rows. Interlaced content is deliberately drawn at field-line pitch over the
woven frame: one gap per emulated line, which is what a 15 kHz set fed an
interlaced signal looks like, rather than one per woven row.

Three classes of frame skip the pass. While a menu or panel is open the CRT
pass would re-draw the UI the compositor just wrote into the buffer, through
a phosphor mask and a curved face, so it is suspended for the same reason
the RTG GPU path above falls back. RTG scanout reaches the surface through
the RTG texture, not the buffer this pass samples. And a programmable
multisync scan (amifb's 31 kHz console, DblPAL, SHRES) has no 15 kHz line
structure to reproduce and no woven fields, so the two-rows-per-line count
would not hold either; `present_programmable` carries that flag out of the
render worker.

A custom shader (`[display] shader = "path.wgsl"`) is checked by naga --
parse, full validation, and a look for the `vs_main`/`fs_main` entry points
-- before any pipeline is built, so a mistake is reported with its WGSL
source location instead of surfacing as a device error later; the file is
size-capped at 1 MiB. It is loaded at window creation, at launcher machine
start, and each time the menu cycles onto `Custom`, which re-reads it from
disk (the live-reload path). A failed load leaves no custom pipeline, and
the selection falls back to `None` with the full diagnostic logged and its
first line shown as an OSD message.

Every capture path bypasses the pass by construction rather than by a check:
screenshots, frame dumps, video recording, CCP capture and the web frontend
all read the CPU presentation buffer, and the shader only ever writes to the
surface. Strength 0 makes every preset's arithmetic an exact identity, but
the pass is still a resample through a plain linear sampler where the
scaler pass uses a texel-snapped sharp bilinear, so it is marginally
softer at magnification than the pass-through; `ShaderKind::None` skips the
pass entirely and is the only zero-cost path.

## Headless capture (`screenshot.rs`)

`--screenshot-after` and `--dump-frames` render through the identical
pipeline with the window hidden; PNGs are scaled to the same geometry the
window would present unless `COPPERLINE_SHOT_RAW=1` requests the unscaled
woven framebuffer. The default vertical presentation scale selects whole
source rows rather than blending adjacent Amiga scanlines, matching the
normal unfiltered display path. Because the default render worker may be one
frame behind, these paths wait for the worker result matching the target
emulated frame before writing the PNG. The
[headless debugger](../debugger/headless) `COPPERLINE_DBG_SHOT` hook reuses
the same path to capture the last completed frame at a breakpoint.

## Video recording (`recorder.rs`)

The [interactive recording](../guide/ui) shortcut writes an AVI containing
lossless ZMBV video -- the DOSBox capture codec: zlib-deflated intra frames
plus XOR-delta inter frames on a
16x16-block grid, encoded entirely with the `flate2` crate -- and
16-bit stereo PCM at the 44.1 kHz mixer rate. `recorder.rs` owns both
the encoder and the AVI muxer, and its unit tests round-trip the stream
through a reference decoder.

Capture is locked to the emulated timeline, not the host clock. Paula
carries an optional capture tap that collects every mixed stereo frame
(before the master output volume); the window drains it once per
emulated frame and, when the frame loop completed a new emulated frame,
waits for the matching presentation buffer before pushing it through the same
`scale_y_into` source-row presentation scale as the live window. At finish the
AVI's video rate/scale is patched from the exact frames-to-audio-samples ratio,
so a nominal "50 fps" label never drifts against PAL's true field rate and
warp-speed captures play back at normal speed. The REC badge, status bar, OSD,
and menus are drawn into the presentation texture after capture, so they never
appear in the file.
