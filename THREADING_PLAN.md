# Render-thread pipeline: design and implementation plan

Status: design only (branch `threaded-render`). Not implemented. The
analysis below was done after the 2026-06-15 host-CPU optimisation pass
(committed to `main`: idle device ticks, background-fill chunking,
poll-stats table). Threading was deliberately *not* landed blind because
its live-window behaviour cannot be verified headlessly; this doc makes the
remaining work mechanical and reviewable with a human at the window.

## Why threading, and the realistic ceiling

The emulator runs the CPU, chipset and the framebuffer paint on a single
thread (winit event loop). A beta tester's borderline machine (A1200/AGA,
68000 clocked to 24.83 MHz = 7x colour clock = 3.5x stock) could not
sustain real time on one core, starving the audio ring buffer -> stutter.

Profiled split on that config (after the committed hot-path pass):

- per-cck chipset/bus arbitration (advance_beam, bitplane_slot_active_at,
  tick_timed_devices, advance_audio, ...): ~60%  -- inherently serial
- CPU emulation (step_slice, bus reads, m68k decode): ~22%
- renderer (bitplane::render): ~12% after the background-fill fix
  (was ~24%; background phase went 1230ms -> 28ms)

The renderer is internally *sequential* (the playfield loop marches a
bitplane-pointer replay cursor down the frame; rows are not independent),
so data-parallel row splitting would break byte-identity. The right design
is a **2-stage pipeline**: emulate frame N on the main thread while a
worker renders frame N-1. Frame wall-time becomes
`max(emulate, render+present)` instead of the sum. With render ~12% the
ceiling is modest now (~10-12%); the bigger structural lever would be
moving *emulation* to the worker and render+present to main, but that
requires routing live input across the thread boundary (see "Alternative").

GPU constraint: wgpu/winit require surface present and window ops on the
**main thread**. So the worker may *paint into a CPU framebuffer* but must
not touch the GPU surface. Present stays on main.

## Determinism is preserved by construction

The renderer already consumes a *completed-frame* snapshot, not live state:
`bus.frame_chip_ram()` returns `last_frame_chip_ram` (a clone taken at the
end-of-frame swap, bus.rs ~5902), plus a timed write journal and recorded
beam events. Rendering is a pure function of that immutable data. If the
worker owns its copy, the emulation thread can advance frame N freely while
the worker paints frame N-1. Save-state / input-replay determinism is
untouched (they depend on the emulation core, not the paint).

## The owned render bundle (`RenderInput`)

`bitplane::render(bus, fb)` currently pulls 17 accessors plus
`RenderState::from_bus(bus)`. To run on a worker, extract them into an owned
struct at frame boundary and add `render_from_input(&RenderInput, fb) ->
VideoRenderFrameTiming`. Then `render(bus, fb)` becomes: build input,
call `render_from_input`, record the returned timing. The headless and
screenshot paths keep calling `render(bus, fb)` and must stay
byte-identical (gate below).

Bundle contents (all owned; clone the slices, move/clone the snapshot):

- geometry: FrameGeometry (Copy)
- visible_start_vpos: u32
- palette_split: (Palette, Palette, bool)
- top_palette_end: Palette
- frame_render_events: Vec<BeamRegisterWrite>      (clone &[])
- current_render_base: RenderRegisterSnapshot
- current_render_events: Vec<BeamRegisterWrite>    (clone &[])
- bottom_palette_events: Vec<BeamRegisterWrite>    (clone &[])
- chip_ram: Vec<u8>                                (2 MB AGA; see handoff)
- chip_ram_writes: Vec<BeamChipRamWrite>           (clone &[])
- captured_bitplane_rows, captured_sprite_lines    (clone; can be large)
- sprite_dma_observed, sprite_display_enable_x_by_y
- render_state: RenderState (from_bus) -- lift its inputs into the bundle
- emulated_seconds, emulated_frames: f64/u64       (for COPPERLINE_DBG_* )

Gotchas:

1. Write-back: `record_video_render_frame(timing)` mutates the bus. On the
   worker, return the timing from `render_from_input` and record it on main.
2. DBG side effects: the playfield export keyed on `emulated_seconds`
   (COPPERLINE_DBG_AFTER/UNTIL) writes files. Pass the scalars in the
   bundle; the file write is fine on the worker but rare. No bus access.
3. Snapshot cost: the bus already clones chip RAM each frame into
   `last_frame_chip_ram`. To avoid a *second* 2 MB clone, hand ownership of
   that buffer to the worker via a double-buffered swap (two Vecs ping-pong
   between bus and worker) rather than cloning into the bundle.

## Pipeline wiring (window.rs)

Persistent worker thread (std::thread, no new deps). Two channels:
`main -> worker` sends `RenderInput`; `worker -> main` returns the painted
+ post-processed presentation framebuffer (and the timing). Per main-loop
iteration:

1. step_frame()  (emulate frame N)
2. build RenderInput for N (or reuse the swapped snapshot buffer)
3. send N to worker
4. recv the finished buffer for N-1 (blocks only if the worker is behind)
5. upload N-1 to the GPU surface and present (main thread)
6. record N-1's returned render timing

The worker runs `render_from_input` then the existing post-process
(center/mask/stretch/deinterlace) into the presentation buffer -- all of
which are already pure framebuffer transforms. One frame of added latency;
acceptable for an emulator (configurable: fall back to synchronous when a
`--no-render-thread` / config flag is set, which also gives an A/B).

## Verification strategy (so it is not landed blind)

1. Byte-identity gate (already built, /tmp/gate equivalents): OCS/ECS/AGA
   boot screenshots must be SHA-identical before/after the `RenderInput`
   refactor. This catches any extraction error.
2. Threaded-dump comparison: add a headless mode that drives the *threaded*
   pipeline with `--dump-frames` and diff every frame against the
   synchronous baseline. This verifies channel ordering / frame skew /
   bundle correctness end-to-end. Only the literal GPU present call (a few
   lines, unchanged) then remains unverified -- low risk.
3. Full unit suite (cargo test, ~971) + clippy + fmt.
4. Human check at the live window: no tearing, input latency acceptable,
   audio underruns gone on a slow machine.

## Alternative (higher ceiling, more risk): emulation on the worker

Move the *emulator* to the worker thread; main runs winit + render +
present. This offloads the ~60% emulation off the UI/GPU thread. Needs:
live keyboard/mouse/menu events routed main -> worker over a channel
(scripted input is already timestamped and could share the path), and the
frame snapshot routed worker -> main. Bigger event-loop refactor; same
determinism argument holds. Consider after the render-pipeline lands and is
measured.
