# The timing model

Copperline's compatibility with cycle-sensitive software comes from one
idea applied consistently: **the chip bus is a single resource arbitrated
per colour clock (CCK), and everyone pays for their slots.** Reference
numbers come from real hardware via the `timing-test/` disk. The Copper
and blitter timing models are documented in full below; the 68000 prefetch
model is in [](cpu). Every rule here is backed by named regression tests in
the inline suites (`src/chipset/copper.rs`, `blitter.rs`, `src/bus.rs`).

## Chip-bus arbitration

Every colour clock of every scanline has an owner, decided in priority
order (`fixed_dma_owner_at`, `src/bus.rs`):

1. **Memory refresh** -- four fixed odd slots early in the line. Refresh
   only ever uses odd slots, which is why it never collides with the
   Copper's even-slot cadence.
2. **Disk DMA** -- three fixed slots, when DSKEN is set and a transfer is
   live.
3. **Audio DMA** -- one slot per Paula channel, claimed only when that
   channel actually has a fetch pending.
4. **Sprite DMA** -- the fixed per-sprite slot pairs.
5. **Bitplane DMA** -- the fetch pattern determined by DDFSTRT/DDFSTOP and
   the plane count; at high plane counts this is what starves everyone
   else, exactly as on hardware.
6. **Copper** -- even-CCK fetch cadence when not waiting.
7. **Blitter** -- any remaining slots its schedule claims.
8. **CPU** -- whatever is left.

The arbiter consults the owner several times per colour clock, so the
bitplane decision (step 5) memoizes its line-invariant part: the effective
DDF window, FMODE fetch cadence, and per-plane fetch-order mask live in a
`BitplaneSlotPlan` keyed on exactly the register inputs that feed them
(`BitplaneSlotKey`, `src/bus.rs`) and are recomputed only when a register
write or a write-delay expiry changes the key. The vpos-dependent gates
(vertical display window, DDFSTRT write miss) are still evaluated live, so
the memoization cannot change behaviour. Once a line reaches DDFSTRT, the
arbiter keeps the fetch sequence anchored there but evaluates BPLCON0 at
each fetch block's first cycle. A later BPLCON0 plane-count increase does
not claim slots for earlier blocks or advance newly enabled plane pointers
for those words, but it can claim the matching slots and start advancing
those pointers on later blocks of the same row.
Wide-FMODE lo-res slots are packed into the first eight CCKs of each
16/32-CCK fetch unit; the rest of the unit remains available to later
arbitration priorities.

Slow RAM at `$C00000` is arbitrated through Agnus *like chip RAM*: a CPU
access to slow RAM contends with DMA even though the RAM is outside the
chip range. (Getting this wrong is observable: too-fast slow RAM can race a
guest DMACON clear against the vertical-blank interrupt.) Fast RAM
and ROM are external-bus accesses billed at the CPU clock without chip-bus
arbitration -- so an accelerated CPU speeds up exactly what a real
accelerator would.

### Deferred timed-device ticks

The chipset and beam advance every colour clock, but the *timed devices*
the CPU can only observe indirectly -- the CIAs, serial port, pots, audio,
floppy, Akiko -- are not ticked per CPU bus access (that dominated the host
profile). Instead their colour clocks accumulate in `pending_device_tick`
and `flush_timed_devices` applies them in one batch at the next observation
boundary: a CIA/custom/peripheral register read or write, or an instruction
boundary for interrupt recognition. Read-only custom registers
(INTREQR/DSKBYTR/SERDATR/POTxDAT) and writes that change device state
(INTREQ/INTENA/ADKCON/DSKLEN/AUDxxx/SERDAT) flush first, so a device always
reflects time right up to the moment it is read or written. The CIA E-clock
divider and every device tick are exact under batching, so observable timing
is unchanged -- the accumulator is a host-CPU optimisation only, and it is
flushed to zero at every frame boundary (so it is never serialized into a
save state).

### CPU vs blitter

DMACON's BLTPRI ("blitter nasty") is modelled as on hardware: with BLTPRI
set the blitter takes every slot it can use and the CPU starves; without
it, the blitter yields a slot after the CPU has missed three consecutive
slots, matching the Minimig RTL. The counter and the back-pressure rule
are detailed under [](#cpu-contention) below.

### Sprite DMA control rewrites

Sprite DMA fetches POS/CTL at the fixed pair slots, then data words for
the active line. Software can still rewrite SPRxPOS/SPRxCTL before a later
pair slot to reposition an already active sprite on that scanline. Those
writes update the live horizontal and vertical comparators, but they do
not restart the sprite data stream: the row offset remains relative to the
descriptor that armed the sprite. Copperline therefore keeps a runtime-only
data-origin VSTART alongside the live comparator VSTART, preserving it
across active POS/CTL rewrites while still using the rewritten HSTART for
the line. The runtime origin is skipped in save states to preserve the
fixed bincode layout; future save-state versioning should serialize it if
mid-line sprite-DMA resume accuracy is tightened. Test:
`active_sprite_control_rewrite_preserves_descriptor_data_origin`.

## The Copper

The Copper (`src/chipset/copper.rs`) is a two-cycle processor that
accesses chip RAM on every *other* colour clock. The HRM describes MOVE as
a two-word, two-memory-cycle instruction; Copperline models the cadence
and its edge cases in detail.

### The fetch cadence and the MOVE write boundary

The two-CCK cadence is locked to the beam's horizontal parity, not a
free-running internal phase: the Copper fetches on even in-line colour
clocks and idles on the odd ones (`Copper::hpos_is_access_cycle`). So:

- A MOVE spans **four colour clocks** (fetch, idle, fetch, idle), leaving
  the alternate clocks free for the blitter/CPU. Modelling the Copper as
  owning every clock while running would starve a chip-bus-bound CPU
  during dense Copper effects such as horizontal colour gradients.
- WAIT and SKIP span **six** colour clocks, spending a dummy-plus-compare
  tail after their two word fetches (the Minimig
  FETCH1/FETCH2/WAITSKIP1/WAITSKIP2 sequence).
- The custom-register side effect occurs on the second word fetch, i.e.
  the third of the four colour clocks: three back-to-back MOVEs starting
  at beam `hpos` write at `hpos + 2`, `hpos + 6`, and `hpos + 10`.

Anchoring the cadence to the beam rather than to a carried-over flip-flop
is what makes a back-to-back colour MOVE list land its writes at the
*same* hpos on every line. With a free-running phase, fixed-DMA
interference drifts the phase line-to-line and a Copper-driven horizontal
gradient shimmers instead of showing clean vertical bands. The parity
check is applied by `Copper::step_eligible_slot`, the single primitive
shared by the live bus path and the blitter-deadline predictor's cloned
simulation, so prediction and execution cannot drift apart.

For the OCS low-res renderer, a same-line `COLORxx` write at beam `hpos`
starts affecting pixels at `(hpos - $36) * 4` (`COLOR_WRITE_HPOS_FB0` in
`src/video/bitplane.rs`); beam-timed placement is anchored at
`COPPER_WAIT_HPOS_FB0` ($28), and bitplane-control writes add the
fetch-to-display pipeline offset (`BITPLANE_CONTROL_PIPELINE_FB`). Because
colour MOVEs are spaced four colour clocks apart, a gradient changes
colour every 8 lores pixels, matching hardware rather than the previous
(too-fine) 4-pixel spacing. Tests:
`copper_move_writes_visible_registers_on_second_dma_slot`,
`copper_move_spends_four_color_clocks_leaving_alternate_cycles_free`.

### WAIT/SKIP edge cases

The Copper compares its masked beam position against the Agnus beam
counters; the BFD bit controls whether a satisfied position wait must also
wait for the blitter to go idle.

- Full-mask waits are satisfied once the beam reaches or passes the target
  in the current 8-bit vertical phase. Near the end of PAL line 255,
  low-half targets wait for the short post-rollover tail while high-half
  targets (e.g. `$fc`) stay satisfied because that phase has passed;
  partial-mask waits with the high vertical bit set likewise stay
  satisfied across the line-255-to-256 rollover.
- With BFD clear a position-satisfied wait parks while the blitter is
  busy and resumes once the scheduled blit consumes its slots; with BFD
  set it ignores a busy blitter.
- The WAIT comparator cannot release in the last 4 colour clocks of a line
  (`WAIT_RELEASE_LINE_END_BLACKOUT_CCK`); a wait satisfied there releases
  at its target on a following line, unless its compare position lies
  *inside* the blackout (the only clock where it is ever true), which
  still releases there.
- The comparator is combinational and runs on **every** colour clock,
  including ones owned by fixed DMA (bitplane/sprite/disk/audio/refresh);
  bus contention only delays the next *fetch*, never the release decision.
  The CDTV extended-ROM boot list depends on this: its `WAIT vp=$FF
  hp=$DE` is only releasable at hpos $DE of line 255, which sits inside
  the last DDFSTOP=$D8 overscan fetch unit -- evaluating the comparator
  only on Copper-eligible slots lost the release and the follow-up
  display-off MOVE. Tests:
  `wait_release_is_blocked_in_line_end_blackout`,
  `copper_wait_comparator_runs_under_fixed_bitplane_dma`.

### COPJMP and frame reload

- A CPU write to COPJMP1/COPJMP2 loads the Copper program counter
  immediately, but the target list has no visible effect until the Copper
  gets DMA slots to fetch it.
- A Copper MOVE can update COP1LC/COP2LC; a later COPJMP strobe branches
  through the *current* value. A Copper MOVE to COPJMP1/COPJMP2 spends its
  second word fetch on the strobe.
- The automatic frame reload latches the current COP1LC at end of frame
  and restarts the Copper at the top of the next frame (vpos 0) through
  the vertical-blank lines -- it branches through a Copper-programmed
  COP1LC value, so a MOVE to COP1LC changes where the next frame restarts.
  A falling-man handoff capture is the regression example recorded in
  `TODO.md` (`t=165s`/`t=180s` frame dumps).

Copper writes to "dangerous" registers are gated by COPCON's CDANG bit.
References: HRM [Coprocessor
Hardware](https://www.theflatnet.de/pub/cbm/amiga/AmigaDevDocs/hard_2.html).

## The blitter

The blitter (`src/chipset/blitter.rs`) is not a do-it-all-at-once engine
with a delay: it is **scheduled per DMA slot**. Each word issues its
hardware channel bus sequence, so DMACONR's BBUSY reflects genuine
in-flight state and BBUSY-polling loops see hardware timing. Because the
renderer and the CPU both need to know when a blit finishes,
`cck_until_blitter_completes` (`src/bus.rs`) predicts completion by walking
the same slot-eligibility primitive the live engine uses.

### Per-slot FSM

Scheduled normal blits use explicit phases matching the hardware
controller: a one-slot BBUSY start delay, an INIT slot, source slots
A/B/C/D, and E/F flush slots for the delayed D holding register. The
source cadence follows the enabled-channel speed table: A is always
visited, B only when enabled, C when enabled (USEC) *or* in fill mode (an
idle C slot, no bus access), and D when D is enabled or no C next-word
state exists. D output is delayed through the hold register: after source
fetches, the first D phase is the HRM "-" bubble and does not claim the
chip bus because no destination word is queued yet. The first destination
word is written on the next D slot and the final word in the F flush slot.
Normal-mode A/B barrel-shifter carry is not cleared by the
BLTSIZE row counter; it carries from the last source word of one row into
the first source word of the next, while masks, modulos, and fill carry
still observe row boundaries. Line blits use L1-L4 phases (L2 latches the
C source word, L3 propagates, L4 stores); line-mode B data loads pass
through the current B shifter at write time, and at completion the
hardware-visible ASH, BSH, SIGN, and low-word BLTAPT accumulator state is
written back. Tests:
`scheduled_normal_mode_bbusy_start_delay_precedes_first_source_slot`,
`blit_pipeline_identifies_idle_cycles_per_hrm_diagrams`,
`scheduled_line_mode_latches_c_source_before_store_phase`,
`scheduled_shift_carry_crosses_normal_mode_row_boundary`.

### Mid-operation register writes

The HRM documents the blitter as an asynchronous DMA engine, says BLTSIZE
starts a blit and must be written last, and tells software to check
BLTDONE before touching blitter registers. Copperline's deterministic
classification of writes while BBUSY is set:

| Register group | Classification | Notes |
| --- | --- | --- |
| `BLTCON0` | Immediate D disable | Clears the current blit's remaining D output path; the register value still updates immediately. |
| `BLTCON1` | Immediate / snapshot-protected | The public register updates immediately, but the in-flight blit keeps its normal/line snapshot so a transient line-mode bit does not reinterpret the active pipeline. |
| `BLTCON0L` (ECS) | Deferred | The minterm-only write drains the current blit before latching. |
| `BLTAFWM`, `BLTALWM` | Deferred | First/last-word masks captured by the scheduled state at BLTSIZE. |
| `BLT[ABCD]PTH/PTL` | Deferred | Pointer writes do not retarget the already-scheduled transfer. |
| `BLT[ABCD]MOD` | Deferred | Modulos captured at BLTSIZE. |
| `BLTBDAT` | Immediate, old-register latch | The first B write after completion zeros the B old register; later writes shift through the latched B data. |
| `BLTSIZE` | Deferred restart | A second start strobe drains the current blit first, then starts the replacement from the post-completion pointer state. |
| `DMACON.DMAEN/BLTEN` | Immediate | Gate blitter bus grants immediately; clearing leaves BBUSY set and preserves the pending blit until re-enabled. |
| `DMACON.BLTPRI` | Immediate | Bus arbitration observes the priority bit directly. |

No covered mid-operation write is modelled as ignored; less common writes
are treated as deferred by draining first. Tests:
`busy_bltcon0_write_disables_remaining_d_output_without_draining_blit`,
`busy_bltsize_write_finishes_current_blit_then_starts_replacement`.

(cpu-contention)=
### CPU contention

With BLTPRI clear, Copperline models the Agnus blitter-slowdown counter
(the Minimig RTL `bls_cnt`). A busy "nice" blitter still holds the chip bus
on its access cycles -- the CPU gets no regular alternate slot. Each colour
clock the waiting CPU misses increments the counter; after
`BLITTER_SLOWDOWN_CPU_MISS_LIMIT` (3) consecutive misses the blitter yields
one slot, matching the HRM "one bus cycle in four" rule and vAmiga's ~2:1
blitter:CPU split on a blitter-heavy 3D-scene regression. Idle blit pipeline cycles
(slots that do not need the bus) never claim the bus and stay
CPU-available even with BLTPRI set, but fixed DMA slots still stall those
idle phases. The counter resets when the CPU gets the bus, when BLTPRI is
set, or when blitter DMA cannot run.

A 68000 `TAS` read-modify-write is unsafe on chip RAM (the HRM warns
against it); the `m68k` backend exposes it as a byte read then a byte
write, so Copperline models `TAS` against chip RAM as two separately
Agnus-arbitrated accesses, each subject to the same back-pressure rule.
Tests: `blithog_clear_busy_blitter_yields_to_cpu_only_after_starvation`,
`bltpri_stalls_cpu_chip_access_through_blitter_access_cycles`.

### Area fill

Area fill is applied only when BLTCON1.DESC is set (the HRM requires
descending mode because the fill carry propagates in descending bit order
across each row); IFE/EFE in ascending mode is treated as ordinary minterm
output. Fill consumes the C-channel slot even when USEC is clear, but as an
**idle cycle** (no bus access) -- the "-" in the HRM "A - D" fill cadence.
So an A->D area fill costs **3 CCK/word** vs 2 for an A->D copy. The fill
carry datapath (`apply_fill` in `finish_source_word`) is not what adds the
cycle -- the C-channel slot in the controller sequence is. Cross-emulator
validated: FS-UAE and vAmiga both time the `bltcon0=0x09F0` A->D fill at 3
CCK/word (timing-test rows 23/24/26). Implemented as `c_phase = use_c ||
fill` with `current_slot_needs_bus` false for the fill C slot; the idle fill
phase advances on CPU/Copper/idle arbitration slots but not through fixed
DMA (bitplane/sprite/disk/audio/refresh) slots. (An earlier
experiment dropped this to 2 CCK/word to improve one capture, but that made
the blitter faster than hardware and broke a separate blitter-heavy
regression.) Test:
`descending_area_fill_costs_one_extra_idle_cycle_per_word`.

### ECS registers

BLTCON0L, BLTSIZV, and BLTSIZH are active only on an ECS Agnus. BLTSIZV
latches the 15-bit vertical size but does not start the blit; BLTSIZH
latches the 11-bit horizontal size and is the ECS start strobe; zero
decodes to the documented maxima (32768 lines, 2048 words). BLTCON1.DOFF
suppresses destination writes while still running the D channel timing,
advancing the D pointer, and updating BZERO from the generated D data.
Tests: `ecs_bltsizv_bltsizh_start_extended_blit`,
`ecs_bltcon1_doff_suppresses_destination_writes_but_advances_pointer`.

### Known residuals

Cross-emulator timing-test comparisons (corroborated in
`timing-test/README.md`) leave one open blitter gap that is tracked
separately:

- **Line blits run slow**: a 64-pixel line measures ~317 beam-CCK in
  Copperline vs 262 on the FS-UAE reference (and 258 predicted by
  `line_total_slots`), roughly 21% too slow.

The previous row-26 display-DMA contention residual is closed: with fixed DMA
stalling idle fill phases, Copperline measures ~25074 CCK for the 3-plane
display fill vs 25208 on the FS-UAE reference.

References: HRM [Blitter
Hardware](https://www.theflatnet.de/pub/cbm/amiga/AmigaDevDocs/hard_6.html).

## Interrupt-recognition latency

A 68000 does not enter an exception the moment INTREQ rises; recognition
plus the exception sequence takes roughly 60-100 CCK on real hardware.
Copperline models this with a configurable latency on newly-raised
interrupt levels (`DEFAULT_IRQ_LATENCY_CCK = 65`, `src/bus.rs`;
`COPPERLINE_IRQ_LATENCY_CCK` overrides, `0` disables).
The delay is attached to Paula/CIA/blitter/Copper source assertions, not to a
CPU write that merely changes INTENA/INTREQ masking or acknowledges a latch.
If software unmasks a source that Paula already latched, the CPU-visible level
updates without creating a fresh asynchronous recognition delay.

This matters more than it sounds: a beam-bounded interrupt handler that
arrives 50 CCK early steals that time from the main loop every frame. The
canonical regression was a scene player running at half speed because
too-early vertical-blank IRQs truncated the depacker's per-frame slice; the
latency model fixed it, confirmed against real hardware with the timing-test
disk.

## Real-time pacing

Pacing never changes emulated behaviour -- it only decides how much
emulated time to advance per host frame and when to sleep. Real mode
debits a per-frame instruction budget one of two ways, selected by
`[emulation] pacing_budget`:

- `cycles` (default): the budget is debited by the chip-bus (device) time
  each slice actually consumed. The cycle-exact core advances the chipset
  through every CPU cycle as it executes -- internal cycles, bus-cycle
  tails, chip-bus grants and contention waits -- so the slice's elapsed bus
  CCK is the true hardware cost (`real_slice_accounting` in
  `src/emulator.rs`). Because the vendored core's 68000 cycle counts are
  accurate to ~1% (SingleStepTests, `vendor/m68k/CYCLE_TIMING_GAP.md`),
  this matches a stock PAL 68000.
- `instructions`: a flat cycles-per-instruction quota
  (`COPPERLINE_REAL_CPU_CPI`, default 4.0), debited by retired
  instructions -- cheaper and pacing-robust, but runs the CPU faster than
  hardware for instruction mixes above the flat cost.

`COPPERLINE_REAL_PACING_BUDGET=cycles|instructions` overrides the config
for one run; the config overrides the built-in `cycles` default.
`COPPERLINE_REAL_PACING_PROFILE=1` emits a one-second pacing log (see
[](peripherals)). Accelerated CPUs scale the budget by
`cpu_clocks_per_cck`; fast RAM/ROM access costs are scaled with sub-CCK
carry accumulation so fractional costs are not lost.

`cycles` became the default once the core's 68000 cycle counts were made
accurate: under flat instruction pacing, or with the old static cycle
counts, a blitter-bound chip-RAM scene had the CPU over-issuing chip-bus
accesses and starving the line blitter. With
accurate per-instruction costs the CPU's chip-bus slot ratio settles at a
physically valid value naturally, which (together with the area-fill C-slot
fix) resolved that blanking regression.

### Thread scheduling priority

Pacing only works if the host actually runs the right thread at the right
moment. Two threads are latency-critical: the **pacer** (the main thread,
which advances the core and calls `thread::sleep` in
`Emulator::sleep_until_realtime_device_time`) and the **cpal audio callback**
(which drains the sample ring buffer the pacer keeps ~150 ms ahead of the
device clock). During live-audio startup and rebuffering, the pacer treats the
unfilled prebuffer as additional required lead; the large-stall self-heal
allows for that lead so it does not cancel the refill as if it were a host
pause. The `copperline-render` worker ([](architecture)) is a
throughput thread, not a latency one, and is left at normal priority. When the
host is busy, a scheduler that preempts the pacer shows up as frame stutter,
and one that preempts the audio callback shows up as an audible underrun.

`[emulation] realtime_priority` (off by default; `COPPERLINE_REALTIME_PRIORITY`
overrides it for one run) asks the OS to schedule those two threads above
normal. It is best effort -- `src/priority.rs` logs what it did and never fails
the run -- and, like all pacing, it never changes emulated behaviour; it only
changes when host work is scheduled. The implementation is per-platform because
"real-time priority" is portable in neither API nor semantics:

- **macOS** -- the pacer thread joins the `USER_INTERACTIVE` QoS class
  (`pthread_set_qos_class_self_np`), the idiomatic unprivileged low-latency
  request. The audio callback is left untouched: Core Audio already runs it on
  a real-time thread, and pinning a QoS class onto it would only *demote* it.
- **Windows** -- both threads are raised to `THREAD_PRIORITY_HIGHEST` via the
  `thread-priority` crate; no privilege required.
- **Linux / other Unix** -- raising priority needs privilege (an `rtprio`
  rlimit, `CAP_SYS_NICE`, or root); without it the request is logged and
  declined, and the thread keeps normal scheduling.

The pacer sleeps between work chunks rather than spinning, so even the
strongest scheduling class it can land in still yields the CPU and cannot
starve the host -- which is why elevating it is safe to offer.

## Cross-checking against hardware

`timing-test/` is a bootable disk that measures CPU and chip-bus operation
timings against the CIA E-clock and reports them on screen and over
serial. The same numbers can be collected from Copperline, vAmiga, FS-UAE,
and real Amigas; several timing fixes (IRQ latency, the area-fill C slot)
were validated this way. When changing the timing model, update the
corresponding reference doc and add a named regression test for the
hardware behaviour.
