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
For DMA-fetched HAM playfields, the display window gates framebuffer output
and collision recording, but the low-res Denise phase can still seed the HAM
component history just before DIW: standard `$38` DDF timing starts the first
visible output one native sample into the fetched stream, so replay
pre-advances that hidden sample before painting the DIW edge. Extra fetch
groups from an earlier DDFSTRT are not decoded into the HAM hold colour before
DIW opens; they are fetched by Agnus, but the first visible HAM history is
bounded to the display-phase samples. Single-word lo-res fetches that start
before the standard `$38` DDF slot expose complete 16-pixel groups; the
standard one-sample phase bias is trimmed when it would push a standard-width
DIW past the completed early-DDF row at the right edge.
Late single-word lo-res DDF keeps the standard DIW `$81` one-sample phase;
the renderer must not subtract an extra sample just to align the clipped
start to a fetch-unit boundary.
When DDFSTRT is late enough that DIW opens before DMA has delivered the
first BPL1DAT word for the row, playfield output remains border-colour until
that plane-0 fetch reaches Denise instead of sampling stale shifter contents.
That gate is placed in the bitplane/DIW coordinate domain, not the normal
Copper/register-write output domain, because it follows the fetch slot that
loads BPL1DAT.
Once that first DMA word is visible, the renderer samples the enabled
bitplanes from the complete latched word; it does not expose the first word
plane-by-plane according to each plane's individual DMA slot.
If a manual BPL1DAT write starts a word before a later DMA BPL1DAT load
point, replay stops the manual word where that DMA word replaces Denise's
shifter.
BPLCON1-delayed samples at the left edge of a contiguous bitplane-DMA block
come from the previous line's shifter tail when current-line DMA is already
feeding Denise at the display edge. Block-start lines, and lines whose output
is held until a delayed first BPL1DAT load, blank that scroll-in because no
current-line shifter data has reached the visible gate yet. AGA's extended
BPLCON1 delays can exceed one 16-bit shifter word; replay does not reuse the
single cached line-tail word for those wider delays, so the extra leading gap
stays background until current-line samples reach Lisa.
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

The mapping from beam coordinates to framebuffer x is anchored by
constants that encode the hardware's fetch-to-display pipeline delays --
register writes, palette writes, and bitplane data each land at their own
documented offset, and the bitplane fetch reference differs between lo-res
and hi-res. Wide-FMODE DMA fetches start from the revision-masked DDFSTRT
comparator value and complete whole units, but the displayed shifter origin
is still quantized by the FMODE fetch gulp; the renderer keeps those two
effects separate. Denise's output line starts at the horizontal blanking
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
origin.

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
`RenderInput::from_bus`. `render_from_input` consumes only that owned
bundle, so the main thread can start emulating frame N+1 while the worker
renders frame N.

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
Progressive content is line-doubled without history.
`COPPERLINE_DEINTERLACE=0` falls back to plain line doubling.
In the default threaded pipeline the worker owns this history; the
synchronous fallback keeps it on the window `App`.

The deinterlacer also hosts the optional CRT phosphor-persistence stage
(`[display] phosphor` / `COPPERLINE_PHOSPHOR`, off by default, clamped to
0.95): when on, `present_with_phosphor` blends each presented frame over a
retained copy of the previous one, keeping `phosphor`/256 of the old value
per channel for an exponential trail. This is what fuses field-rate flicker
(alternate-field dither transparency, flicker-dithered animation) the way a
real tube does. Like the rest of the deinterlacer it operates on the
presentation buffer only and never touches the emulated framebuffer.

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

## Presentation (`video/window.rs`, `video/ui.rs`)

`window.rs` owns the winit `ApplicationHandler` and the `pixels` GPU
surface: the field is presented at a TV-like 4:3 aspect plus the
44-pixel status bar, scaling continuously with the window. The GPU surface
is fed from `present_fb`, the post-processed presentation buffer produced by
either the render worker or the synchronous fallback.

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
- **Full-overscan horizontal recentring**: in `"full"` presentation, a standard
  (non-overscan) display is recentred because the framebuffer captures a deep
  slab of left overscan that would otherwise push the picture right of centre.
  The decision keys off the bitplane data the display actually fetches (DDF),
  not just the DIW window: a demo that opens DIW wide around a standard-width
  picture (Virtual Dreams' "Absolute Inebriation") is still recentred, while a
  display that genuinely fetches bitplane data into the overscan border is left
  exactly as rendered.

`ui.rs` implements the status bar widgets, the pop-up menu, the smaller
overlay panels (About, Shortcuts, Calibration), and the shared debugger/tool
panel drawing used by the native debugger and frame-analyzer windows. The UI
uses the 8x8 `font.rs` glyphs. `COPPERLINE_UI_PREVIEW=1 cargo test
panels_render_into_their_rects` renders every panel into
`target/ui-preview-*.png` -- the screenshots in this documentation come
from there -- and the `test_app()` fixture drives the debugger window
against a real emulator instance in the unit tests.

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
`scale_y_into` source-row presentation scale as screenshots. At finish the
AVI's video rate/scale is patched from the exact frames-to-audio-samples
ratio, so a nominal "50 fps" label never drifts against PAL's true field rate
and warp-speed captures play back at normal speed. The REC badge, status bar,
OSD, and menus are drawn into the presentation texture after capture, so they
never appear in the file.
