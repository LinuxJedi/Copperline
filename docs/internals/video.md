# The video pipeline

The renderer's central rule: **it never races the chipset.** The chipset
does not paint pixels as it runs; instead, every render-relevant event is
recorded with its beam position, and the renderer replays the completed
frame's events afterwards. The live emulation and the painting of pixels
are decoupled in time but exact in beam position.

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

The playfield pixel loop runs in control-run chunks: recorded control,
scroll, and palette events take effect at output-pixel boundaries, so
between two event positions everything derived from `ControlState` (the
BPLCON0 mode decode, display-window edges, fetch-origin quantization,
per-plane scroll delays) is constant and is computed once per run rather
than per pixel. The per-pixel decisions inside a run are unchanged -- the
chunking is a host-CPU optimisation, not a model change.

The mapping from beam coordinates to framebuffer x is anchored by
constants that encode the hardware's fetch-to-display pipeline delays --
register writes, palette writes, and bitplane data each land at their own
documented offset, and the bitplane fetch reference differs between lo-res
and hi-res. These anchors were calibrated against real-hardware captures
and other emulators; `COPPERLINE_HCENTER=0` and `COPPERLINE_OVERSCAN=full`
help when re-checking them.

The framebuffer is a 716x285 overscan field (lo-res pixels doubled
horizontally). It captures deep overscan on all sides.

Two vertical edge cases the replay honours:

- A display window can open above the captured canvas (the canvas top
  follows DIWSTRT down to a minimum start line). Bitplane pointers are
  pre-advanced for those clipped rows by replaying the frame's
  BPLCON0/DMACON writes line by line, so only lines where bitplane DMA
  was actually enabled consume a row -- the CDTV boot screen opens its
  window at line 5 but raises BPLCON0 from 0 to 6 planes at line 24.
- Canvas rows whose beam line lies at or past the frame wrap (the fixed
  285-row field is taller than a standard PAL scan) are forced to black:
  the beam never produces those lines, and a deep-overscan window would
  otherwise let the replay keep walking bitplane memory past the image.

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
44-pixel status bar, scaling continuously with the window.

Two presentation-only adjustments (they never alter the emulated
framebuffer):

- **Overscan mask**: `[display] overscan = "tv"` masks deep-overscan
  margins in black like a CRT bezel; `"full"` shows the entire field.
- **Horizontal recentring**: a standard (non-overscan) display is recentred
  for presentation, since the framebuffer captures a deep slab of left
  overscan that would otherwise push the picture right of centre compared
  with vAmiga/FS-UAE. Overscan frames are left exactly as rendered.

`ui.rs` implements the status bar widgets, the pop-up menu, and the
overlay windows (About, Shortcuts, Calibration, Debugger) drawn over the
display with the 8x8 `font.rs` glyphs. `COPPERLINE_UI_PREVIEW=1 cargo test
panels_render_into_their_rects` renders every panel into
`target/ui-preview-*.png` -- the screenshots in this documentation come
from there -- and the `test_app()` fixture drives the debugger window
against a real emulator instance in the unit tests.

## Headless capture (`screenshot.rs`)

`--screenshot-after` and `--dump-frames` render through the identical
pipeline with the window hidden; PNGs are scaled to the same geometry the
window would present. The [headless debugger](../debugger/headless)
`COPPERLINE_DBG_SHOT` hook reuses the same path to capture the last
completed frame at a breakpoint.

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
pushes the deinterlacer output through the same `scale_y_into`
presentation resample as screenshots. At finish the AVI's video
rate/scale is patched from the exact frames-to-audio-samples ratio, so
a nominal "50 fps" label never drifts against PAL's true field rate and
warp-speed captures play back at normal speed. The REC badge, status
bar, OSD, and menus are drawn into the presentation texture after
capture, so they never appear in the file.
