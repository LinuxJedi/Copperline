# CPU integration

## The wrapper and the bus adapter

`M68kMachine` (`src/cpu.rs`) wraps the vendored pure-Rust `m68k` core
(`crates/m68k/`). The core sees
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
set in true 80-bit extended precision via a pure-Rust software floating-
point engine (`crates/m68k/src/fpu/softfloat.rs`). Arithmetic (add, sub,
mul, div, sqrt), ordered compare, round-to-integer, scale, getexp/getman,
the format conversions, FMOVE/FMOVEM in every operand format (including
packed decimal), the constant ROM, FBcc/FScc/FDBcc/FTRAPcc, control
registers, the FPCR rounding mode/precision and the FPSR exception/accrued
bytes, and the FSAVE/FRESTORE state frames (NULL after reset, 68881-style
IDLE once touched) are all modelled. The transcendentals (FSIN/FCOS/FTAN,
FASIN/FACOS/FATAN, the hyperbolics, FETOX/FETOXM1/FTWOTOX/FTENTOX,
FLOGN/FLOGNP1/FLOG2/FLOG10) and FSINCOS run in extended precision too: a
double-`FloatX80` ("double-double", ~128-bit) layer
(`crates/m68k/src/fpu/dd.rs`) evaluates Taylor/atanh series over reduced
ranges and rounds the result to extended under the FPCR mode, setting INEX
and the domain flags (OPERR/DZ). Accuracy is validated against an
arbitrary-precision oracle (the pure-Rust `astro-float`, a dev-only
dependency; `crates/m68k/tests/fpu_accuracy.rs`): every function is within
1 ULP across a wide sweep and all four rounding modes, and round-to-nearest
is correctly rounded in practice. They are not chip-bit-exact -- the real
6888x uses its own CORDIC/polynomial microcode, and on a bare 68040 these
trap to a software FPSP. FMOD/FREM compute the exact remainder and the FPSR
quotient byte. This covers Kickstart's
detection and per-task FPU context switching. The
68000's per-instruction cycle counts in the vendored core have been
corrected against the SingleStepTests corpus to ~1% aggregate accuracy
(see `crates/m68k/CYCLE_TIMING_GAP.md`), which is what makes
cycle-budgeted pacing trustworthy.

## Prefetch

The 68000's two-word instruction prefetch queue (IRD/IRC) is modelled in
the vendored core (`prefetch_queue` in `crates/m68k/src/core/cpu.rs`): the
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
that have them: the instruction cache on the 68020/68EC020/68030/68040 and the
data cache on the 68030/68040 (`CpuModel::has_instruction_cache`/
`has_data_cache`). CACR (and CAAR on the 020/030) are always stored; software
(AmigaOS at boot) enables and clears the cache exactly as on hardware. A cache
hit costs no bus cycle, so a cached instruction fetch does not contend with
chip-bus DMA -- which is the point: 020/030/040 code looping out of chip RAM
otherwise pays a bitplane-DMA arbitration stall on every fetch and runs at
roughly half speed, drifting an AGA demo's interrupt-driven music or animation
to half its intended rate. The data cache only covers expansion RAM/ROM
because chip and slow RAM are DMA-visible and cache-inhibited, as on real
machines. A 68000/68010 models no cache. `[cpu] icache = false`/`dcache =
false` opt a cached CPU back out; with no cache modelled, the cache-control
instructions are no-ops and self-modifying code always executes fresh bytes
(the safe direction).

Each cache is a power-of-two array of direct-mapped longword entries
(`src/cache.rs`): 64 entries (256 bytes) on the 020/030 -- the exact 68020
instruction-cache geometry -- and 1024 (4 KB) on the 68040. The larger 040
capacity is the part that matters here: a chip-RAM loop bigger than 256 bytes
stays resident on a 040 where it would thrash a 020. The 040's 4-way
set-associative, 16-byte-line, copyback organisation is deliberately not
modelled, and need not be -- copyback is unobservable because the data cache
only covers expansion RAM, which is not DMA-visible, so write-back versus
write-through cannot be told apart. The 040 also redefines CACR (only the IE/DE
enable bits, no freeze or clear strobes) and moves invalidation to the
CINV/CPUSH instructions; the model maps a CINV/CPUSH to a whole-cache clear of
the indicated cache(s) -- over-clearing line/page scopes, which is always
coherent (a push has no dirty data to write back in a write-through model).

The instruction cache does not snoop writes (authentic 68020 behaviour), so a
line stays valid until DMA or the CPU rewrites its backing memory, or software
clears it (via CACR on the 020/030, via CINV/CPUSH on the 040). Restoring a
save state that predates cache modelling (or was captured with the cache
disabled) re-establishes the model's cache cold and re-derives its enable bits
from the restored CACR, so the machine keeps the cache its CPU has rather than
silently running cacheless after a load.

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
`scale_cycles_for_cpu_type` (`crates/m68k/src/core/cpu.rs`) derives 020+
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
which is the whole chip-slot cost at the native 14 MHz ratio. On Alice, reads,
writes, and custom-register reads all consume that granted slot without an
additional colour-clock bubble; adding one over-stalls AGA 020 chip-RAM
read/modify/write loops, while the A1200 timing-test chip-read row remains
aligned without it. On OCS/ECS machines the 020 still talks to the 16-bit chip
bus, so chip/slow/custom reads pay a one-CCK data-return wait when the
3-clock short bus cycle is otherwise hidden inside a single colour-clock slot.
The tail's fractional cck are carried so none are lost; the 68000/010 keep the
full 4-clock (2-cck) cycle (`Bus::cpu_short_bus_cycle`). Residuals: per-frame
throughput still runs below the reference, and the cycle model does not reflect
instruction-cache hit/miss *latency* (only its bus-traffic effect), so software
that toggles CACR cache-on/off and depends on the exact transition timing can
diverge.

## MMU

The 68030 and 68040 model the on-chip MMU (`has_pmmu`), so software that sets up
and enables address translation runs rather than having its MMU writes ignored.
The two parts differ enough to need separate walkers: the 68030 uses its
programmable table walk (CRP/SRP selection, the TC index fields, 4-/8-byte
descriptor modes, early-termination descriptors), while the 68040 has a
fixed-format three-level walk (root -> pointer -> page, 4 KB or 8 KB pages,
URP/SRP split by supervisor) keyed off its own TC layout. Both share the
transparent translation registers (TTRs: the 030's TT0/1, the 040's ITT0/1 +
DTT0/1), which short-circuit the walk for a matching address range.

One register set (`mmu_*` in `CpuCore`) is canonical for both paths, so the 030
`PMOVE` writes, the 040 `MOVEC` writes, and the walker can never desync (the 040
root pointers overload the CRP/SRP address slots, dispatched by `cpu_type`). The
enable bit differs by part -- TC[31] on the 030, TC[15] on the 040 -- via
`tc_enable()`. `PMOVE`/`MOVEC` load the registers and `PTEST`/`PLOAD`/`PFLUSH`
are accepted as no-ops rather than trapping.

A 68040 table walk costs three descriptor fetches, far too much to pay on every
access, so the 040 walker is fronted by an address-translation cache
(`crate::mmu::atc`, mirrored on the real 64-entry ATC): a direct-mapped cache of
(logical page -> physical page) keyed by page frame and supervisor flag. It is a
pure cache -- never serialized (restored empty) -- and is flushed when the
mapping could change: a write to TC / a root pointer / a TTR, or a PFLUSH. Like
real hardware a plain write to a descriptor does not auto-flush; software must
PFLUSH, which is where the ATC is cleared.

A table walk that misses a valid descriptor currently falls back to identity
translation rather than raising an access fault: the resumable 68040 access-error
and 68030 long-bus-fault stack frames are not built yet, so faulting mid-access
would crash the guest -- identity is the safe direction (the same stance the
caches take). Remaining work: real access faults with write-protect/supervisor
permission checks and MMUSR reporting (so Enforcer-class tools work).

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
