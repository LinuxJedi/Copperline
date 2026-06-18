# CPU integration

## The wrapper and the bus adapter

`M68kMachine` (`src/cpu.rs`) wraps the vendored pure-Rust `m68k` core
(`vendor/m68k/`). The core sees
the machine through an adapter implementing its `AddressBus` trait, so
every CPU-visible access -- RAM, ROM, custom registers, CIA, RTC,
autoconfig, Gayle, Akiko -- routes into the shared `Bus` and is billed in
colour clocks:

- Chip and slow RAM go through chip-bus arbitration
  (`grant_cpu_bus_access`) and genuinely wait for free slots.
- Fast RAM, ROM, and other external-bus targets are billed at the CPU
  clock (`cpu_external_access`), scaled by `cpu_clocks_per_cck` with
  sub-CCK carry so accelerated clocks bill fractional costs exactly.
- Addresses are masked to the model's bus width: 24-bit for
  68000/68EC020, 32-bit for 68020/030/040.

Selectable models: 68000, 68EC020, 68020, 68030, 68040. `[cpu] fpu`
fits a 68881/68882 to any 020/030 (and is on by default for the 68040,
whose FPU is on-die): the vendored core executes the 6888x instruction
set in f64 precision -- arithmetic, transcendentals, FMOVE/FMOVEM in
every operand format except packed decimal, FBcc/FScc/FDBcc/FTRAPcc,
the constant ROM, control registers, and FSAVE/FRESTORE state frames
(NULL after reset, 68881-style IDLE once touched), which is what
Kickstart's detection and per-task FPU context switching exercise. The
68000's per-instruction cycle counts in the vendored core have been
corrected against the SingleStepTests corpus to ~1% aggregate accuracy
(see `vendor/m68k/CYCLE_TIMING_GAP.md`), which is what makes
cycle-budgeted pacing trustworthy.

## Prefetch

The 68000's two-word instruction prefetch queue (IRD/IRC) is modelled in
the vendored core (`prefetch_queue` in `vendor/m68k/src/core/cpu.rs`): the
next opcode is fetched before the current instruction finishes, so
self-modifying code that overwrites the *next* instruction executes the
stale pre-write word (real MC68000 Class 1 SMC behaviour), while a taken
branch flushes and refills the stream from the target. The queue lives in
the backend core rather than a Copperline-side bus cache, because correct
flushing depends on the CPU's own control flow, exceptions, and
interrupts. It is gated to the 68000 (`prefetch_enabled`); 68010+ fetch
directly at PC through the bus adapter. Chip-RAM probes pin both cases:
`cpu_prefetch_probe_documents_self_modified_next_opcode_behavior` (stale
fall-through) and
`cpu_prefetch_probe_branch_refetches_self_modified_chip_ram_target`
(branch refetch).

## Caches

The on-chip caches are silicon, so they are modelled by default on the parts
that have them: the instruction cache on the 68020/68EC020/68030 and the data
cache on the 68030 (`CpuModel::has_instruction_cache`/`has_data_cache`).
CACR/CAAR are always stored; software (AmigaOS at boot) enables and clears the
cache through CACR exactly as on hardware. A cache hit costs no bus cycle, so
a cached instruction fetch does not contend with chip-bus DMA -- which is the
point: 020/030 code looping out of chip RAM otherwise pays a bitplane-DMA
arbitration stall on every fetch and runs at roughly half speed, drifting an
AGA demo's interrupt-driven music or animation to half its intended rate. The
data cache only covers expansion RAM/ROM because chip and slow RAM are
DMA-visible and cache-inhibited, as on real machines. A 68000/68010 models no
cache. `[cpu] icache = false`/`dcache = false` opt a 020/030 back out; with no
cache modelled, the cache-control instructions are no-ops and self-modifying
code always executes fresh bytes (the safe direction).

The instruction cache does not snoop writes (authentic 68020 behaviour), so a
line stays valid until DMA or the CPU rewrites its backing memory, or software
clears it via CACR. Restoring a save state that predates cache modelling (or
was captured with the cache disabled) re-establishes the model's cache cold
and re-derives its enable bits from the restored CACR, so the machine keeps
the cache its CPU has rather than silently running cacheless after a load.

## 68020+ timing

The 68000/68010 cycle counts are validated against the
[SingleStepTests](https://github.com/SingleStepTests) (TomHarte) corpus,
but **no equivalent vectors exist for the 68020, 68030, or 68040** -- the
project publishes cycle-accurate vectors for the 68000 only. That is not
an oversight: the 020+ parts have an instruction cache, a three-stage
pipeline, and dynamic bus sizing, so there is no single cycle-exact count
per instruction and therefore no widely-trusted reference to generate
vectors from.

Copperline handles this by approximation rather than precision.
`scale_cycles_for_cpu_type` (`vendor/m68k/src/core/cpu.rs`) derives 020+
timing from the corrected 68000 counts: the pipeline and cache make most
instructions cost roughly half their 68000 cycles, with a two-cycle
floor, and memory-bound work is dominated by the host bus model anyway.
The flat scale is wrong for a few instructions whose 020 cost does not
track the 68000 count, so those carry an explicit 020 cycle value (still
pre-scale): the barrel-shifter shift/rotate, the fixed-cost MULU/MULS, a
taken `DBcc`'s pipeline refill, and `MOVE` (register vs memory-source
read latency). These are calibrated against a cycle-exact A1200 reference
(FS-UAE) using the `timing-test/` ADF -- with the instruction cache
enabled, since that is the A1200 default; `timing-test/compare.py` checks
each row against the reference. This is good enough because Copperline
paces to wall-clock time and models the CPU:chipset clock ratio and
chip-bus arbitration exactly; per-instruction 020 cycles matter far less
than those for Amiga software, which is overwhelmingly 68000. The
68000/68010 paths are left untouched so the TomHarte-validated timing is
never disturbed. If 020+ accuracy ever becomes a real requirement, the
Motorola 68020/030 user-manual timing tables or differential testing
against Moira/Musashi are the realistic sources.

On the A1200 the 020 chip-bus timing is calibrated against a cycle-exact
FS-UAE A1200 (the 6/8 scaling above, a 32-bit Alice chip-bus data path, and
a two-entry longword fetch latch). The 020's chip-bus cycle is modelled as 3
CPU clocks, not the 68000's 4: after the granted colour-clock slot the access
bills only the shorter remaining tail (one clock -- half a cck at the stock
2-clock ratio, none at 14 MHz where the 3-clock cycle fits inside one slot),
which is write-posting -- a write hands off to the chip bus and the CPU runs
on. A read cannot: it stalls until the slow chip bus returns the data, so a
020 read additionally bills a one-cck data-return wait
(`Bus::bill_020_read_data_wait`, chip/slow/custom reads only). At the stock
2-clock ratio that wait is hidden under the instruction's own cycle total, but
at 14 MHz it dominates, making a chip read 8 CPU cycles rather than 6 and
matching the FS-UAE A1200 reference exactly. The tail's fractional cck are
carried so none are lost; the 68000/010 keep the full 4-clock (2-cck) cycle and
no data-return wait (`Bus::cpu_short_bus_cycle`). Residuals: per-frame throughput still runs below
the reference, and the cycle model does not reflect instruction-cache
hit/miss *latency* (only its bus-traffic effect), so software that toggles
CACR cache-on/off and depends on the exact transition timing can diverge.
MMU-dependent 030/040 accelerator setups are a non-goal.

## Interrupts and STOP

Paula's INTENA/INTREQ levels are delivered as M68K autovectors through the
modelled recognition latency described in [](timing). When the CPU
executes `STOP`, the frame loop fast-forwards device time to the next
event that can raise an interrupt instead of spinning -- behaviour the
debugger's Step control inherits.

## Exceptions

If the guest triggers something unimplemented (an exotic custom-register
edge case, say), the CPU may halt with an `EXCEPTION`. This is non-fatal:
the window stays alive showing the last framebuffer, and the debugger can
inspect the halted state.
