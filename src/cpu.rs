// SPDX-License-Identifier: GPL-3.0-or-later

//! M68K CPU wrapper and CPU-visible Amiga bus adapter.

use crate::bus::{Bus, CpuBusAccessKind};
use crate::chipset::paula::{pending_ipl, INT_MASTER};
use crate::config::CpuModel;
use crate::memory::{
    ACCEL_RAM_BASE, AUTOCONFIG_BASE, AUTOCONFIG_SIZE, CHIP_RAM_BASE, CHIP_WINDOW_SIZE, ROM_BASE,
    SLOW_RAM_BASE, WCS_BASE,
};
use crate::savestate::{chunk, split};
use anyhow::{anyhow, Result};
use log::{debug, trace};
use m68k::core::memory::{BusFault, BusFaultKind};
use m68k::{AddressBus, BatchExit, CpuCore, CpuType, FastMem, StepResult};

pub const CIA_A_BASE: u32 = 0x00BF_E000;
pub const CIA_A_SIZE: u32 = 0x0000_1000;
pub const CIA_B_BASE: u32 = 0x00BF_D000;
pub const CIA_B_SIZE: u32 = 0x0000_1000;
pub const CUSTOM_BASE: u32 = 0x00DF_F000;
pub const CUSTOM_SIZE: u32 = 0x0000_1000;
pub const RTC_BASE: u32 = 0x00DC_0000;
pub const RTC_SIZE: u32 = 0x0001_0000;
/// Gayle gate array: IDE + status/interrupt registers at $DA0000-$DAFFFF
/// and the ID shift register page at $DE1000.
pub const GAYLE_BASE: u32 = 0x00DA_0000;
pub const GAYLE_SIZE: u32 = 0x0001_0000;
pub const GAYLE_ID_BASE: u32 = 0x00DE_1000;
pub const GAYLE_ID_SIZE: u32 = 0x0000_1000;
/// CDTV battery-backed bookmark RAM (top half of the RTC page).
pub const CDTV_BATTRAM_BASE: u32 = 0x00DC_8000;
pub const CDTV_BATTRAM_SIZE: u32 = 0x0000_8000;

const ADDRESS_MASK_24BIT: u32 = 0x00FF_FFFF;
const ADDRESS_MASK_32BIT: u32 = 0xFFFF_FFFF;
const AUTOVECTOR_SENTINEL: u32 = 0xFFFF_FFFF;
/// Safety cap on COPPERLINE_DBG_TRACE output so an un-windowed trace cannot run away.
const DEBUG_TRACE_LINE_CAP: u64 = 200_000;

fn sync_cck_enabled_setting() -> bool {
    #[cfg(feature = "internal-diagnostics")]
    {
        !crate::envcfg::flag("COPPERLINE_NO_SYNC_CCK")
    }
    #[cfg(not(feature = "internal-diagnostics"))]
    {
        true
    }
}

fn diag_cpu_sync_on() -> bool {
    use std::sync::OnceLock;
    static V: OnceLock<bool> = OnceLock::new();
    *V.get_or_init(|| crate::envcfg::flag("COPPERLINE_DIAG_CPU_SYNC"))
}

fn diag_cpu_sync_pc_ranges() -> &'static [(u32, u32)] {
    use std::sync::OnceLock;
    static V: OnceLock<Vec<(u32, u32)>> = OnceLock::new();
    V.get_or_init(|| {
        let Some(spec) = crate::envcfg::var("COPPERLINE_DIAG_CPU_SYNC_PC") else {
            return Vec::new();
        };
        spec.split(',')
            .filter_map(|part| {
                let part = part.trim();
                if let Some((lo, hi)) = part.split_once(':') {
                    let lo = crate::envcfg::parse_u32(lo)?;
                    let hi = crate::envcfg::parse_u32(hi)?;
                    Some((lo.min(hi), lo.max(hi)))
                } else {
                    let pc = crate::envcfg::parse_u32(part)?;
                    Some((pc, pc))
                }
            })
            .collect()
    })
}

fn diag_cpu_sync_pc_matches(pc: u32) -> bool {
    let ranges = diag_cpu_sync_pc_ranges();
    ranges.is_empty() || ranges.iter().any(|(lo, hi)| (*lo..=*hi).contains(&pc))
}

/// Entries kept in the debugger's recent-PC ring.
pub const UI_PC_HISTORY_CAP: usize = 64;

/// A runtime instruction trace started from the console: disassembled
/// lines with beam positions, written through a buffered file.
struct UiTrace {
    writer: std::io::BufWriter<std::fs::File>,
    path: std::path::PathBuf,
    lines: u64,
    cap: u64,
}

pub struct M68kMachine {
    cpu: CpuCore,
    bus: CpuBus,
    fpu_enabled: bool,
    /// Fast batch/trace-JIT execution (`[cpu] jit`). When set and no
    /// per-instruction debug hook is armed, `step_slice` runs the core's
    /// instruction-budgeted batch path instead of cycle-stepping, billing
    /// each instruction a flat bus-cycle cost. Configuration, not machine
    /// state: like `fpu_enabled` it is never serialized.
    jit_enabled: bool,
    /// Last CACR value pushed into the cache models, so the per-instruction
    /// sync is a single compare when nothing changed.
    last_cacr: u32,
    // BTreeMap, not HashMap: the diagnostic dumps sort by count only, so
    // equal-count entries must fall back to a deterministic PC order.
    dbg_pc_hist: std::collections::BTreeMap<u32, u64>,
    dbg_pc_cyc: std::collections::HashMap<u32, u64>,
    dbg_pc_on: bool,
    dbg_in_window: bool,
    dbg_pc_dumped: bool,
    // COPPERLINE_DIAG_IPL: per-frame CPU cycles spent in interrupt handlers (SR IPL
    // mask > 0) vs main code (IPL 0), to see how much of the per-frame budget
    // interrupts consume.
    dbg_ipl_main_cyc: u64,
    dbg_ipl_irq_cyc: u64,
    dbg_ipl_last_frame: u64,
    // COPPERLINE_DIAG_CRASH: ring buffer of (pc, opcode, a7) used to dump the
    // execution path when the CPU first runs into zero-opcode memory.
    dbg_crash_ring: std::collections::VecDeque<(u32, u16, u32)>,
    dbg_crash_on: bool,
    dbg_crash_dumped: bool,
    // COPPERLINE_DBG_*: interactive-style debugger (breakpoints/watchpoints/trace).
    dbg: Option<crate::debugger::Debugger>,
    // Debugger-window breakpoints/watchpoints. Owned here so they stay
    // armed while the window is closed; ui_stop is the pending hit the
    // window polls and surfaces (pause + reopen the debugger).
    ui_breaks: crate::debugger::InteractiveBreaks,
    ui_stop: Option<crate::debugger::DebugStop>,
    /// ThisTask pointer at the last armed task-catch check, so only a
    /// reschedule (a change) can fire the catch. Debug-only state.
    ui_last_this_task: Option<u32>,
    /// LoadSeg observer behind the armed loadseg catch. Debug-only
    /// state, never serialized; reset whenever the catch is toggled.
    ui_loadseg_tracker: crate::amigaos::LibraryTracker,
    /// Recent retired-instruction PCs, a debugger-only ring recorded
    /// while a debug window is open ("how did I get here").
    ui_pc_history: [u32; UI_PC_HISTORY_CAP],
    ui_pc_history_next: usize,
    ui_pc_history_len: usize,
    ui_pc_history_enabled: bool,
    /// Console-started instruction trace (TRACE START/STOP), None when
    /// off; per-instruction cost only while tracing.
    ui_trace: Option<UiTrace>,
    /// Host-side precise CPU sampler used by `profile.start`. Like the other
    /// debugger observers it never enters a save state and forces the precise
    /// instruction loop while armed.
    #[cfg(feature = "control")]
    profile_samples: Option<crate::profile::samples::InstructionSampler>,
    // COPPERLINE_DBG_SPREN: previous DMACON, to detect the instruction that
    // clears the sprite-DMA-enable bit.
    dbg_prev_dmacon: u16,
    // COPPERLINE_DBG_FC=ADDR(hex): previous value of a watched word read via the
    // CPU's own address decode, to measure how often a watched frame counter
    // increments (and at what rate).
    dbg_prev_fc: u16,
    dbg_fc_count: u64,
    // COPPERLINE_DIAG_SCHED: trace a cooperative scheduler loop. Logs the beam
    // position and frame counter at the loop top and at the frame-skip nudge
    // check, so we can see how much of a frame one scheduler iteration consumes
    // and whether it crosses a vblank.
    // When set, advance the chipset clock through each instruction's CPU-internal
    // cycles (not just its bus accesses) so bus_cck tracks cpu_cck -- DMA phase and
    // frame timing then match real hardware. Diagnostic builds can disable this
    // for A/B comparison. Validated against the timing-test ROM
    // (shift/multiply/per-frame throughput now match FS-UAE) and
    // cycle-sensitive display DMA workloads.
    sync_cck_on: bool,
    /// CPU clocks per colour clock. A stock 68000 is 2 (7.09 MHz / 3.55 MHz);
    /// an accelerated CPU advances the chipset by fewer colour clocks per CPU
    /// cycle, so chip/slow RAM stays chip-bus bound while fast RAM and CPU
    /// internal work run at the higher clock. Defaults to 2.
    cpu_clocks_per_cck: u32,
    /// Sub-cck remainder from converting per-instruction CPU clocks to colour
    /// clocks. Carried across instructions so an accelerated CPU (many clocks
    /// per cck) is not rounded up to a whole cck on every instruction.
    cpu_clock_carry: u32,
    dbg_sched_on: bool,
    dbg_sched_top_vpos: u32,
    dbg_sched_top_frame: u64,
    dbg_sched_top_counter: u16,
    dbg_sched_render_vpos: u32,
    dbg_sched_render_frame: u64,
    dbg_sched_b8_vpos: u32,
    dbg_sched_b8_frame: u64,
    // Histogram of PCs executed during the render-done -> task-B-return span, to
    // see what the second cooperative task does while the window overruns.
    dbg_sched_capturing: bool,
    dbg_sched_bhist: std::collections::BTreeMap<u32, u64>,
    dbg_sched_bhist_dumped: bool,
    dbg_sched_loop_dumped: bool,
    // Interrupt handler-duration probe: per-IPL-level entry state (index =
    // level, vector = level + 24; return_pc == 0 means not in flight), plus
    // a per-PC cck billing table for the level-6 body with its one-shot
    // cost-table dump.
    dbg_sched_vec_return_pc: [u32; 8],
    dbg_sched_vec_entry_cck: [u64; 8],
    dbg_sched_vec_entry_vpos: [u32; 8],
    dbg_sched_vec_entry_hpos: [u32; 8],
    dbg_sched_vec30_prev_pc: u32,
    dbg_sched_vec30_prev_cck: u64,
    dbg_sched_vec30_cost: std::collections::HashMap<u32, (u64, u64)>,
    dbg_sched_vec30_runs: u32,
    dbg_sched_vec30_cost_dumped: bool,
    // Cached per-instruction diagnostic gates (read from the environment once at
    // construction). These run on every instruction, so they must not do a live
    // std::env lookup -- that lock+scan, millions of times a second, drops the
    // emulator well below real time. None/false disables with a single branch.
    dbg_fc_addr: Option<u32>,
    dbg_ipl_on: bool,
    dbg_spren_on: bool,
}

#[derive(Debug, Clone, Copy)]
pub struct CpuStepSlice {
    pub instructions: usize,
    pub cpu_cycles: u32,
    pub cpu_cck: u32,
    pub bus_advanced_cck: u32,
    pub stopped: bool,
}

#[derive(Debug, Clone, Copy)]
struct IrqDispatch {
    cycles: i32,
    #[cfg(feature = "control")]
    level: u8,
    #[cfg(feature = "control")]
    vector: u16,
}

/// Exact FP0-FP7 extended values followed by FPCR, FPSR and FPIAR.
pub type DebugFpuRegisters = ([(u16, u64); 8], u32, u32, u32);

/// Bincode wrappers naming the save-state component in errors (bincode's
/// boxed error does not satisfy anyhow's `.context` bounds directly).
fn serialize_component<W: std::io::Write, T: serde::Serialize>(
    w: &mut W,
    value: &T,
    what: &str,
) -> Result<()> {
    bincode::serialize_into(w, value).map_err(|e| anyhow!("serializing {what}: {e}"))
}

fn deserialize_component<R: std::io::Read, T: serde::de::DeserializeOwned>(
    r: &mut R,
    what: &str,
) -> Result<T> {
    crate::savestate::deserialize_from_state(r).map_err(|e| anyhow!("reading {what}: {e}"))
}

/// The CPU-level diagnostic observers `M68kMachine::arm_diagnostic_hooks`
/// switches on, one field per environment knob. All default to off.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct DiagnosticHooks {
    /// `COPPERLINE_DBG_MEMW`: CPU-write watch on one word.
    pub memw_addr: Option<u32>,
    /// `COPPERLINE_DBG_FC`: log every change of the word at this address.
    pub frame_counter_addr: Option<u32>,
    /// `COPPERLINE_DBG_SPREN`: report DMACON sprite-enable clears.
    pub spren_clear: bool,
    /// `COPPERLINE_DBG_IRQ` with its `(after, until)` window in seconds.
    pub irq_window: Option<(f64, f64)>,
    /// `COPPERLINE_DIAG_IPL`: per-frame handler/main cycle split.
    pub ipl_split: bool,
    /// `COPPERLINE_DIAG_PCHIST`: PC histogram over its window.
    pub pc_histogram: bool,
    /// `COPPERLINE_DIAG_CRASH`: recent-PC ring for crash context.
    pub crash_ring: bool,
}

/// Machine-level runtime state outside `CpuCore` and `Bus` that a save
/// state must carry: cache-sync memo, timing carries, and clock scaling.
/// The `dbg_*` fields are host instrumentation and stay live across a load.
#[derive(serde::Serialize, serde::Deserialize)]
struct MachineRuntimeState {
    last_cacr: u32,
    sync_cck_on: bool,
    cpu_clocks_per_cck: u32,
    cpu_clock_carry: u32,
}

/// Same-process rollback also needs the interrupt sample consumed at the next
/// instruction boundary. File saves intentionally omit these adapter latches.
#[derive(serde::Serialize, serde::Deserialize)]
struct MachineRollbackState {
    bus: crate::bus::RollbackState,
    sampled_irq_level: u8,
    ipl_sample_held: bool,
}

/// The components a state parses into before the machine swaps onto them.
struct RestoredState {
    cpu: CpuCore,
    runtime: MachineRuntimeState,
    icache: Option<Box<crate::cache::CpuCache>>,
    dcache: Option<Box<crate::cache::CpuCache>>,
    bus: Bus,
}

/// Longest 68000-family instruction worth attributing to one PC when
/// marking executed code: opcode plus full extension words. A larger
/// jump between instruction boundaries is a branch or an exception, not
/// a straight-line advance, so only the opcode word is marked.
const MAX_INSTRUCTION_BYTES: u32 = 12;

/// Instruction budget per `run_batch` call in the JIT slice. Interrupts are
/// only recognized between batches (plus once inside each batch entry), so
/// this bounds interrupt latency in JIT mode to a small fraction of a
/// scanline of billed time. Larger batches let compiled traces run longer
/// between host round-trips but delay interrupt delivery; AROS boot at
/// accelerated clock ratios proved sensitive to latencies beyond about a
/// scanline.
const JIT_BATCH_INSTRUCTIONS: usize = 64;

/// Flat CPU-clock cost billed per instruction retired by the JIT slice.
/// The batch path reports no cycle counts, so this constant sets the JIT
/// machine's nominal speed: one instruction per clock models an ideal
/// pipelined accelerator, making guest throughput track `[cpu] clock_mhz`
/// directly (a 50 MHz 68040 runs ~50 MIPS in fast RAM). Deterministic by
/// construction -- a truly host-speed "fastest possible" mode would make
/// emulated results depend on the host, which Copperline never allows.
const JIT_CPU_CLOCKS_PER_INSTRUCTION: u32 = 1;

struct CpuBus {
    bus: Bus,
    address_mask: u32,
    // COPPERLINE_DBG_MEMW=ADDR(hex): watch a 16-bit word in chip/slow RAM and record
    // the last CPU write that touches it, so the run loop can log the writer PC.
    dbg_memw_addr: Option<u32>,
    dbg_memw_hit: Option<u16>,
    /// 68020/030 instruction cache model, present only when `[cpu] icache`
    /// opts in (the calibrated default timing assumes no cache model).
    icache: Option<Box<crate::cache::CpuCache>>,
    /// 68030 data cache model (`[cpu] dcache`).
    dcache: Option<Box<crate::cache::CpuCache>>,
    /// COPPERLINE_DBG_IRQ cached time window. Interrupt acknowledge can be hot
    /// during IRQ storms, so parse the environment once at construction.
    dbg_irq_window: Option<(f64, f64)>,
    /// Set by the sized-access fall-through around the recursive byte read
    /// for the odd second byte of an aligned word: an unmapped byte read
    /// with this set rides the 16-bit bus cycle its even partner already
    /// billed. Live only inside one access, never carried across a save
    /// state.
    unmapped_read_continues_word: bool,
    /// Whether the most recent instruction-stream read hit the icache
    /// model. The 68060 timing model gates superscalar pairing and branch
    /// folding on a cached fetch stream. Recomputed on every fetch, never
    /// carried across a save state.
    last_fetch_cache_hit: bool,
    /// True only if every opcode/extension/immediate fetch in the current
    /// instruction hit the icache. The 68020 timing tables' cache case is
    /// defined for the complete instruction being resident, not merely its
    /// opcode word.
    instruction_fetches_cached: bool,
    /// IPL level as sampled at the CPU's most recent bus access. The real
    /// 68000 latches its IPL pins during bus cycles, and the take-interrupt
    /// decision at an instruction boundary uses the value latched during the
    /// PREVIOUS instruction's last bus access -- a level that rises during an
    /// instruction's trailing internal cycles is not seen until one
    /// instruction later. Transient: re-sampled on every access, never
    /// serialized in file saves (a loaded state re-samples within one instruction).
    /// Exact rollback captures it in `MachineRollbackState`.
    sampled_irq_level: u8,
    /// Set by the core's `ipl_hold_sample` poll-point marker: the current
    /// `sampled_irq_level` is the instruction's microcode poll value and
    /// later accesses in the same instruction must not re-latch it (e.g.
    /// RMW instructions poll during the final prefetch, BEFORE their
    /// writeback). Cleared when the boundary decision consumes the sample.
    /// File-save transient like `sampled_irq_level`; captured for rollback.
    ipl_sample_held: bool,
    /// Start PC of the instruction currently inside the core, used only by
    /// `COPPERLINE_DIAG_CPU_SYNC` to filter internal-cycle trace lines.
    diag_current_pc: u32,
    /// Mirror of `M68kMachine::jit_enabled` so `fast_mem` can gate the
    /// zero-cost fastmem window without reaching back up to the machine.
    jit_enabled: bool,
}

pub fn build(
    bus: Bus,
    cpu_model: CpuModel,
    fpu_enabled: bool,
    cpu_clocks_per_cck: u32,
    unimplemented: crate::config::UnimplementedPolicy,
    _track_stopped_slice_progress: bool,
) -> Result<M68kMachine> {
    let mut machine = M68kMachine::new(bus, cpu_model, fpu_enabled)?;
    machine.cpu.emulate_unimplemented_060 =
        unimplemented == crate::config::UnimplementedPolicy::Native;
    if cpu_model == CpuModel::M68060 && unimplemented == crate::config::UnimplementedPolicy::Trap {
        log::info!(
            "68060 unimplemented instructions will trap faithfully: the OS \
             needs 68060.library (or set [cpu] unimplemented = \"native\")"
        );
    }
    machine.cpu_clocks_per_cck = cpu_clocks_per_cck.max(1);
    machine
        .bus
        .bus
        .set_cpu_clocks_per_cck(machine.cpu_clocks_per_cck);
    Ok(machine)
}

impl M68kMachine {
    pub fn new(bus: Bus, cpu_model: CpuModel, fpu_enabled: bool) -> Result<Self> {
        let mut cpu = CpuCore::new();
        cpu.set_cpu_type(cpu_type_for_model(cpu_model));
        cpu.address_mask = address_mask_for_model(cpu_model);
        if cpu_model == CpuModel::M68060 && !fpu_enabled {
            // An FPU-less 060 (LC/EC060) presents as PCR.DFP: the core
            // raises the faithful format $4 disabled frame instead of the
            // host's generic Line-F shim for discrete 68881/68882 absence.
            cpu.pcr |= m68k::PCR_DFP;
        }
        // A 68020/030 without an attached 68881/68882 routes cpID-1 F-line
        // opcodes to the Line-F exception inside the core. The precise path
        // additionally pre-empts them with `force_fpu_line_f_if_needed`
        // (which also covers the 68040, where the core models FPU absence
        // via the LC/EC types instead of this flag); the core-side flag is
        // what makes the batch/JIT path honour a missing FPU.
        cpu.fpu_present = fpu_enabled;

        let mut machine = Self {
            cpu,
            bus: CpuBus {
                bus,
                address_mask: address_mask_for_model(cpu_model),
                dbg_memw_addr: crate::envcfg::var_os("COPPERLINE_DBG_MEMW").and_then(|s| {
                    s.to_str()
                        .map(|s| s.trim().trim_start_matches("0x"))
                        .and_then(|s| u32::from_str_radix(s, 16).ok())
                }),
                dbg_memw_hit: None,
                icache: None,
                dcache: None,
                dbg_irq_window: debug_irq_window_setting(),
                unmapped_read_continues_word: false,
                last_fetch_cache_hit: false,
                instruction_fetches_cached: false,
                sampled_irq_level: 0,
                ipl_sample_held: false,
                diag_current_pc: 0,
                jit_enabled: false,
            },
            fpu_enabled,
            jit_enabled: false,
            last_cacr: 0,
            dbg_ipl_main_cyc: 0,
            dbg_ipl_irq_cyc: 0,
            dbg_ipl_last_frame: 0,
            dbg_pc_hist: std::collections::BTreeMap::new(),
            dbg_pc_cyc: std::collections::HashMap::new(),
            dbg_in_window: false,
            dbg_pc_on: crate::envcfg::flag("COPPERLINE_DIAG_PCHIST"),
            dbg_pc_dumped: false,
            dbg_crash_ring: std::collections::VecDeque::with_capacity(65),
            dbg_crash_on: crate::envcfg::flag("COPPERLINE_DIAG_CRASH"),
            dbg_crash_dumped: false,
            dbg: crate::debugger::Debugger::from_env(address_mask_for_model(cpu_model)),
            ui_breaks: crate::debugger::InteractiveBreaks::new(address_mask_for_model(cpu_model)),
            ui_stop: None,
            ui_last_this_task: None,
            ui_loadseg_tracker: crate::amigaos::LibraryTracker::default(),
            ui_pc_history: [0; UI_PC_HISTORY_CAP],
            ui_pc_history_next: 0,
            ui_pc_history_len: 0,
            ui_pc_history_enabled: false,
            ui_trace: None,
            #[cfg(feature = "control")]
            profile_samples: None,
            dbg_prev_dmacon: 0,
            dbg_prev_fc: 0,
            dbg_fc_count: 0,
            sync_cck_on: sync_cck_enabled_setting(),
            cpu_clocks_per_cck: 2,
            cpu_clock_carry: 0,
            dbg_sched_on: crate::envcfg::flag("COPPERLINE_DIAG_SCHED"),
            dbg_sched_top_vpos: 0,
            dbg_sched_top_frame: 0,
            dbg_sched_top_counter: 0,
            dbg_sched_render_vpos: 0,
            dbg_sched_render_frame: 0,
            dbg_sched_b8_vpos: 0,
            dbg_sched_b8_frame: 0,
            dbg_sched_capturing: false,
            dbg_sched_bhist: std::collections::BTreeMap::new(),
            dbg_sched_bhist_dumped: false,
            dbg_sched_loop_dumped: false,
            dbg_sched_vec_return_pc: [0; 8],
            dbg_sched_vec_entry_cck: [0; 8],
            dbg_sched_vec_entry_vpos: [0; 8],
            dbg_sched_vec_entry_hpos: [0; 8],
            dbg_sched_vec30_prev_pc: 0,
            dbg_sched_vec30_prev_cck: 0,
            dbg_sched_vec30_cost: std::collections::HashMap::new(),
            dbg_sched_vec30_runs: 0,
            dbg_sched_vec30_cost_dumped: false,
            dbg_fc_addr: crate::envcfg::var("COPPERLINE_DBG_FC")
                .and_then(|s| u32::from_str_radix(s.trim().trim_start_matches("0x"), 16).ok()),
            dbg_ipl_on: crate::envcfg::flag("COPPERLINE_DIAG_IPL"),
            dbg_spren_on: crate::envcfg::flag("COPPERLINE_DBG_SPREN"),
        };
        // The 68020+ has a 3-clock chip-bus cycle (vs the 68000's 4); tell the
        // bus so it bills the shorter post-grant tail (write-posting).
        let short_bus = !machine.cpu.is_pre_68020;
        machine.bus.bus.set_cpu_short_bus_cycle(short_bus);
        machine.reset_cpu();
        Ok(machine)
    }

    // COPPERLINE_DIAG_PCHIST: histogram the CPU PC during the gear/loading window
    // (38..42s emulated) to locate the DMS decrunch hot loop, then dump the top
    // PCs (with the instruction words there) once the window closes.
    fn dbg_record_pc(&mut self) {
        let secs = self.bus.bus.emulated_seconds();
        let start = crate::envcfg::var("COPPERLINE_DIAG_PCHIST_START")
            .and_then(|s| s.parse::<f64>().ok())
            .unwrap_or(38.0);
        self.dbg_in_window = (start..start + 4.0).contains(&secs);
        if self.dbg_in_window {
            *self.dbg_pc_hist.entry(self.cpu.pc).or_insert(0) += 1;
        } else if secs >= start + 4.0 && !self.dbg_pc_dumped && !self.dbg_pc_hist.is_empty() {
            self.dbg_pc_dumped = true;
            let total: u64 = self.dbg_pc_hist.values().sum();
            let mut top: Vec<(u32, u64)> =
                self.dbg_pc_hist.iter().map(|(&pc, &n)| (pc, n)).collect();
            top.sort_by_key(|b| std::cmp::Reverse(b.1));
            log::info!(
                "pchist gear-window total_samples={total} unique_pc={}",
                top.len()
            );
            for (pc, n) in top.iter().take(30) {
                let w0 = self.bus.bus.peek_word_any(*pc);
                let w1 = self.bus.bus.peek_word_any(pc.wrapping_add(2));
                let pct = *n as f64 / total as f64 * 100.0;
                let cyc = self.dbg_pc_cyc.get(pc).copied().unwrap_or(0);
                let cyc_per = cyc as f64 / *n as f64;
                log::info!("pchist PC={pc:#010X} n={n:8} ({pct:5.2}%) cyc/instr={cyc_per:5.2} words={w0:04X} {w1:04X}");
            }
        }
    }

    // COPPERLINE_DIAG_CRASH: keep a short history of (pc, opcode, A7) and dump
    // it when the CPU executes a sustained run of zero opcodes from chip RAM
    // (real empty memory -- a single 0x0000 opcode is a valid ORI.B #imm,D0) or
    // when a blit is armed to write low memory. Catches "demo jumped into empty
    // memory" and exception-vector corruption crashes with the path that led
    // there.
    fn dbg_record_crash_path(&mut self) {
        if self.dbg_crash_dumped {
            return;
        }
        let pc = self.cpu.pc;
        let opcode = self.bus.bus.peek_word_any(pc);
        // Require a run of zero words at and after the PC: 8 consecutive zero
        // words cannot be real code (ORI.B #0,D0 spam), only empty memory.
        let pc_in_chip_ram = (pc as usize) < self.bus.bus.mem.chip_ram.len();
        let in_empty_memory = pc_in_chip_ram
            && opcode == 0
            && (1..8).all(|k| self.bus.bus.peek_chip_word(pc as usize + k * 2) == 0);
        let came_from_code = self
            .dbg_crash_ring
            .back()
            .map(|&(_, op, _)| op != 0)
            .unwrap_or(false);
        // Also trigger when the bus flags a blit aimed at the vector table /
        // low memory: dump the code path that programmed it.
        let lowmem_blit = self.bus.bus.diag_lowmem_blit;
        if lowmem_blit {
            self.bus.bus.diag_lowmem_blit = false;
        }
        if lowmem_blit || (in_empty_memory && came_from_code) {
            self.dbg_crash_dumped = true;
            let secs = self.bus.bus.emulated_seconds();
            let reason = if lowmem_blit {
                "low-memory blitter destination"
            } else {
                "zero-opcode chip RAM"
            };
            log::warn!(
                "CRASH: {reason} at PC={:#010X} t={:.4}s SR={:#06X}",
                pc,
                secs,
                self.cpu.get_sr(),
            );
            log::warn!(
                "  INTREQ={:#06X} INTENA={:#06X} DMACON={:#06X} blitter_busy={}",
                self.bus.bus.paula.intreq,
                self.bus.bus.paula.intena,
                self.bus.bus.agnus.dmacon,
                self.bus.bus.blitter.busy,
            );
            if lowmem_blit {
                let b = &self.bus.bus.blitter;
                let line = b.bltcon1 & 0x0001 != 0;
                let writes = if line { "C-gated line" } else { "D-channel" };
                log::warn!(
                    "  blit {writes} con0={:#06X} con1={:#06X} apt={:#08X} bpt={:#08X} cpt={:#08X} dpt={:#08X} dmod={}",
                    b.bltcon0,
                    b.bltcon1,
                    b.bltapt,
                    b.bltbpt,
                    b.bltcpt,
                    b.bltdpt,
                    b.bltdmod,
                );
            }
            for reg in 0..8 {
                log::warn!(
                    "  D{reg}={:#010X} A{reg}={:#010X}",
                    self.cpu.d(reg),
                    self.cpu.a(reg)
                );
            }
            let sp = self.cpu.a(7);
            let mut stack = String::new();
            for k in 0..24 {
                stack.push_str(&format!(
                    "{:04X} ",
                    self.bus.bus.peek_word_any(sp.wrapping_add(k * 2))
                ));
            }
            log::warn!("  stack @A7={sp:#010X}: {stack}");
            // Hexdump the code around the crash site and around the most recent
            // distinct PC pages in the history, so operands (not just opcodes)
            // are visible.
            let mut bases: Vec<u32> = self
                .dbg_crash_ring
                .iter()
                .map(|&(p, _, _)| p & !0x3F)
                .collect();
            bases.push(pc & !0x3F);
            bases.sort_unstable();
            bases.dedup();
            for base in bases {
                let mut row = String::new();
                for k in 0..32 {
                    row.push_str(&format!(
                        "{:04X} ",
                        self.bus.bus.peek_word_any(base.wrapping_add(k * 2))
                    ));
                }
                log::warn!("  mem {base:#010X}: {row}");
            }
            for &(p, op, a7) in self.dbg_crash_ring.iter() {
                log::warn!("  path: PC={p:#010X} op={op:04X} A7={a7:#010X}");
            }
        }
        self.dbg_crash_ring.push_back((pc, opcode, self.cpu.a(7)));
        if self.dbg_crash_ring.len() > 64 {
            self.dbg_crash_ring.pop_front();
        }
    }

    /// COPPERLINE_DBG_*: per-instruction debugger work done before the CPU steps.
    /// Handles instruction tracing and PC breakpoints, and (when watchpoints
    /// are configured) returns a snapshot of the watched memory words so the
    /// post-step pass can detect writes. Returns `Some(empty)` when an Exec-list
    /// check still needs the post-step pass, and `None` when no post-step work
    /// is active.
    fn debug_before_step(&mut self) -> Option<Vec<(u32, u16)>> {
        let secs = self.bus.bus.emulated_seconds();
        if !self.dbg.as_ref().is_some_and(|d| d.enabled_at(secs)) {
            return None;
        }
        self.maybe_dump_copper(secs);
        self.maybe_dump_ram(secs);
        self.maybe_arm_headless_alert_break(secs);
        self.debug_check_headless_exception(secs);
        let pc = self.cpu.pc;

        if let Some((trace_full, lo, hi)) = self
            .dbg
            .as_ref()
            .filter(|d| d.trace)
            .map(|d| (d.trace_full, d.trace_lo, d.trace_hi))
        {
            let in_window = pc >= lo && pc <= hi;
            let trace_lines = self.dbg.as_ref().map(|d| d.trace_lines).unwrap_or(0);
            if in_window && trace_lines < DEBUG_TRACE_LINE_CAP {
                let bus = &self.bus.bus;
                let (text, _len) = crate::disasm::disassemble(
                    |addr| bus.peek_word_any(addr),
                    pc,
                    self.cpu.cpu_type,
                );
                if trace_full {
                    // COPPERLINE_DBG_TRACE_FULL: a fixed-width, all-hex record of
                    // the full register file and CCR, for line-by-line diffing
                    // against a reference 68000 trace. `op` is the raw opcode word.
                    let op = self.bus.bus.peek_word_any(pc);
                    let d: Vec<String> = (0..8).map(|i| format!("{:08X}", self.cpu.d(i))).collect();
                    let a: Vec<String> = (0..8).map(|i| format!("{:08X}", self.cpu.a(i))).collect();
                    log::info!(
                        "ft pc={pc:06X} op={op:04X} ccr={:02X} \
                         d={} {} {} {} {} {} {} {} a={} {} {} {} {} {} {} {} | {text}",
                        self.cpu.get_ccr(),
                        d[0],
                        d[1],
                        d[2],
                        d[3],
                        d[4],
                        d[5],
                        d[6],
                        d[7],
                        a[0],
                        a[1],
                        a[2],
                        a[3],
                        a[4],
                        a[5],
                        a[6],
                        a[7],
                    );
                } else {
                    log::info!(
                        "dbg trace t={secs:.5} pc={pc:#010X} {text:<30} \
                         d0={:#X} d1={:#X} d2={:#X} d7={:#X} a0={:#X} a1={:#X} a2={:#X} a4={:#X} a7={:#X}",
                        self.cpu.d(0),
                        self.cpu.d(1),
                        self.cpu.d(2),
                        self.cpu.d(7),
                        self.cpu.a(0),
                        self.cpu.a(1),
                        self.cpu.a(2),
                        self.cpu.a(4),
                        self.cpu.a(7),
                    );
                }
                if let Some(d) = self.dbg.as_mut() {
                    d.trace_lines += 1;
                }
            }
        }

        if self.dbg.as_ref().is_some_and(|d| d.alert_break == Some(pc)) {
            let code = self.cpu.d(7);
            let decoded = crate::debugger::guru_decode(code);
            self.debug_report(
                &format!("ALERT pc={pc:#010X} code={code:#010X} {decoded}"),
                secs,
            );
        } else if self.dbg.as_ref().is_some_and(|d| d.is_breakpoint(pc)) {
            self.debug_report(&format!("BREAK pc={pc:#010X}"), secs);
        }

        let watch_words: Vec<u32> = self
            .dbg
            .as_ref()
            .map(|d| {
                d.watches
                    .iter()
                    .flat_map(|w| {
                        let base = w.addr & !1;
                        let count = ((w.addr & 1) + w.len).div_ceil(2);
                        (0..count).map(move |k| base + k * 2)
                    })
                    .collect()
            })
            .unwrap_or_default();
        let listcheck_pending = self
            .dbg
            .as_ref()
            .is_some_and(|d| !d.listcheck.is_empty() && !d.listcheck_reported);
        if watch_words.is_empty() && !listcheck_pending {
            return None;
        }
        Some(
            watch_words
                .into_iter()
                .map(|addr| (addr, self.bus.bus.peek_word_any(addr)))
                .collect(),
        )
    }

    fn debug_check_headless_exception(&mut self, secs: f64) {
        let Some(vector) = self.cpu.last_exception_vector else {
            return;
        };
        let Ok(vector) = u16::try_from(vector) else {
            return;
        };
        if !self.dbg.as_ref().is_some_and(|d| d.catches_vector(vector)) {
            return;
        }
        self.cpu.last_exception_vector = None;
        let name = crate::debugger::exception_vector_name(vector);
        let frame = self.debug_exception_frame_summary(vector);
        self.debug_report(&format!("CATCH {name} vector={vector} {frame}"), secs);
    }

    fn debug_exception_frame_summary(&self, vector: u16) -> String {
        let sp = self.cpu.a(7);
        let word = |off: u32| self.bus.bus.peek_word_any(sp.wrapping_add(off));
        let stacked_sr = word(0);
        let stacked_pc = (u32::from(word(2)) << 16) | u32::from(word(4));
        let opcode = self.bus.bus.peek_word_any(stacked_pc);
        let mut stack = String::new();
        for k in 0..8 {
            stack.push_str(&format!(" {:04X}", word(k * 2)));
        }

        if matches!(vector, 2 | 3) && self.cpu.cpu_type == CpuType::M68000 {
            let access = (u32::from(word(6)) << 16) | u32::from(word(8));
            return format!(
                "frame_sr={stacked_sr:#06X} frame_pc={stacked_pc:#010X} \
                 frame_op={opcode:04X} access={access:#010X} stack@a7={sp:#010X}:{stack}"
            );
        }

        if self.cpu.cpu_type == CpuType::M68000 {
            return format!(
                "frame_sr={stacked_sr:#06X} frame_pc={stacked_pc:#010X} \
                 frame_op={opcode:04X} stack@a7={sp:#010X}:{stack}"
            );
        }

        let format_vector = word(6);
        format!(
            "frame_sr={stacked_sr:#06X} frame_pc={stacked_pc:#010X} \
             frame_op={opcode:04X} fmtvec={format_vector:#06X} stack@a7={sp:#010X}:{stack}"
        )
    }

    fn maybe_arm_headless_alert_break(&mut self, secs: f64) {
        if !self
            .dbg
            .as_ref()
            .is_some_and(|d| d.catch_alert && d.alert_break.is_none())
        {
            return;
        }
        let bus = &self.bus.bus;
        let peek8 = |addr: u32| {
            let word = bus.peek_word_any(addr & !1);
            if addr & 1 == 0 {
                (word >> 8) as u8
            } else {
                word as u8
            }
        };
        let peek32 = |addr: u32| {
            (u32::from(bus.peek_word_any(addr)) << 16)
                | u32::from(bus.peek_word_any(addr.wrapping_add(2)))
        };
        let os = crate::amigaos::OsMemory {
            peek8: &peek8,
            peek32: &peek32,
        };
        let Ok(execbase) = os.exec_base() else {
            return;
        };
        // exec.library Alert() is at LVO -108. The jump-table entry itself
        // executes with D7 holding the alert code, so a PC break there reports
        // the same information as the interactive CATCHALERT command.
        let pc = execbase.wrapping_sub(108) & self.cpu.address_mask;
        if let Some(dbg) = self.dbg.as_mut() {
            dbg.alert_break = Some(pc);
        }
        log::info!(
            "dbg catchalert armed: execbase={execbase:#010X} alert_pc={pc:#010X} t={secs:.5}"
        );
    }

    /// COPPERLINE_DBG_COPPER: disassemble the active Copper list once, the first
    /// time the debugger is active. Reads through the CPU's own memory decode
    /// so chip-RAM mirrors resolve, and stops at the end-of-list WAIT.
    fn maybe_dump_copper(&mut self, secs: f64) {
        let Some(req) = self
            .dbg
            .as_ref()
            .filter(|d| !d.copper_dumped)
            .and_then(|d| d.copper_dump.clone())
        else {
            return;
        };
        let cop1lc = self.bus.bus.agnus.cop1lc;
        let coppc = self.bus.bus.copper.pc();
        // "auto" prefers the COP1LC reload pointer (the list start); if a
        // program has not set it yet, fall back to the live Copper PC.
        let start = req.addr.unwrap_or(if cop1lc != 0 { cop1lc } else { coppc });
        let bus = &self.bus.bus;
        let lines = crate::disasm::dump_copper_list(
            |addr| bus.peek_word_any(addr),
            start,
            req.count as usize,
        );
        log::info!(
            "dbg copper list @ {start:#010X} ({} instrs) cop1lc={cop1lc:#010X} coppc={coppc:#010X} t={secs:.5}",
            lines.len()
        );
        for (addr, text) in &lines {
            log::info!("  {addr:#010X}: {text}");
        }
        if let Some(d) = self.dbg.as_mut() {
            d.copper_dumped = true;
        }
    }

    /// COPPERLINE_DBG_RAMDUMP: write a memory region to a file once, the first
    /// time the debugger is active. Reads through the CPU's own memory decode
    /// so chip-RAM mirrors resolve.
    fn maybe_dump_ram(&mut self, secs: f64) {
        let Some(req) = self
            .dbg
            .as_ref()
            .filter(|d| !d.ram_dumped)
            .and_then(|d| d.ram_dump.clone())
        else {
            return;
        };
        let bus = &self.bus.bus;
        let mut data = Vec::with_capacity(req.len as usize);
        let mut addr = req.addr & !1;
        while (data.len() as u32) < req.len {
            let word = bus.peek_word_any(addr);
            data.push((word >> 8) as u8);
            data.push(word as u8);
            addr = addr.wrapping_add(2);
        }
        data.truncate(req.len as usize);
        match std::fs::write(&req.path, &data) {
            Ok(()) => log::info!(
                "dbg ram dump: {} bytes from {:#08X} -> {} t={secs:.5}",
                req.len,
                req.addr,
                req.path
            ),
            Err(e) => log::warn!("dbg ram dump to {} failed: {e}", req.path),
        }
        if let Some(d) = self.dbg.as_mut() {
            d.ram_dumped = true;
        }
    }

    /// Compare watched memory against the pre-step snapshot and report writes.
    /// COPPERLINE_DBG_SPREN: when the DMACON sprite-DMA-enable bit transitions from
    /// set to clear, log the instruction that did it (ppc) and the full register
    /// set, so the scene-transition trigger that kills the wolf can be traced.
    /// Bounded by COPPERLINE_DBG_AFTER/UNTIL emulated seconds.
    /// COPPERLINE_DBG_FC=ADDR(hex): log every change of the 16-bit word at ADDR
    /// (read via the CPU's own address decode, so memory mirrors are honored),
    /// with emulated time, plus a running change count. Used to measure a demo
    /// frame counter's increment rate. Bounded by COPPERLINE_DBG_AFTER/UNTIL.
    fn debug_check_frame_counter(&mut self) {
        let Some(addr) = self.dbg_fc_addr else {
            return;
        };
        let val = self.bus.peek_word(addr);
        if val == self.dbg_prev_fc {
            return;
        }
        self.dbg_prev_fc = val;
        let secs = self.bus.bus.emulated_seconds();
        let after = crate::envcfg::var("COPPERLINE_DBG_AFTER")
            .and_then(|s| s.trim().parse::<f64>().ok())
            .unwrap_or(0.0);
        let until = crate::envcfg::var("COPPERLINE_DBG_UNTIL")
            .and_then(|s| s.trim().parse::<f64>().ok())
            .unwrap_or(f64::INFINITY);
        if secs >= after && secs < until {
            self.dbg_fc_count += 1;
            log::info!(
                "fc {addr:#08X}={val:#06X} ({val}) secs={secs:.5} f={} writer_pc={:#010X} change#{}",
                self.bus.bus.emulated_frames(),
                self.cpu.ppc,
                self.dbg_fc_count,
            );
        }
    }

    /// COPPERLINE_DBG_MEMW=ADDR(hex): when the watched word is written by the CPU,
    /// log the writer PC, the new value, emulated time and frame. Bounded by
    /// COPPERLINE_DBG_AFTER/UNTIL. Catches the actual instruction (self.cpu.ppc is
    /// the PC of the instruction that just executed the write).
    fn debug_check_memw(&mut self) {
        let Some(val) = self.bus.dbg_memw_hit.take() else {
            return;
        };
        let secs = self.bus.bus.emulated_seconds();
        let after = crate::envcfg::var("COPPERLINE_DBG_AFTER")
            .and_then(|s| s.trim().parse::<f64>().ok())
            .unwrap_or(0.0);
        let until = crate::envcfg::var("COPPERLINE_DBG_UNTIL")
            .and_then(|s| s.trim().parse::<f64>().ok())
            .unwrap_or(f64::INFINITY);
        if secs >= after && secs < until {
            let addr = self.bus.dbg_memw_addr.unwrap_or(0);
            log::info!(
                "memw {addr:#08X}={val:#06X} ({val}) secs={secs:.5} f={} writer_pc={:#010X}",
                self.bus.bus.emulated_frames(),
                self.cpu.ppc,
            );
        }
    }

    /// COPPERLINE_DIAG_IPL: accumulate CPU cycles by whether the instruction ran in
    /// an interrupt handler (IPL mask > 0) or main code (IPL 0), and log the split
    /// at each frame boundary. Bounded by COPPERLINE_DBG_AFTER/UNTIL.
    fn debug_check_ipl(&mut self, ipl_before: u16, cycles: u32) {
        if !self.dbg_ipl_on {
            return;
        }
        if ipl_before > 0 {
            self.dbg_ipl_irq_cyc = self.dbg_ipl_irq_cyc.saturating_add(cycles as u64);
        } else {
            self.dbg_ipl_main_cyc = self.dbg_ipl_main_cyc.saturating_add(cycles as u64);
        }
        let frame = self.bus.bus.emulated_frames();
        if frame != self.dbg_ipl_last_frame {
            let secs = self.bus.bus.emulated_seconds();
            let after = crate::envcfg::var("COPPERLINE_DBG_AFTER")
                .and_then(|s| s.trim().parse::<f64>().ok())
                .unwrap_or(0.0);
            let until = crate::envcfg::var("COPPERLINE_DBG_UNTIL")
                .and_then(|s| s.trim().parse::<f64>().ok())
                .unwrap_or(f64::INFINITY);
            if secs >= after && secs < until {
                log::info!(
                    "ipl frame={} main_cyc={} irq_cyc={}",
                    self.dbg_ipl_last_frame,
                    self.dbg_ipl_main_cyc,
                    self.dbg_ipl_irq_cyc,
                );
            }
            self.dbg_ipl_main_cyc = 0;
            self.dbg_ipl_irq_cyc = 0;
            self.dbg_ipl_last_frame = frame;
        }
    }

    /// COPPERLINE_DIAG_SCHED: trace a cooperative scheduler iteration. Called
    /// with the PC about to execute. At the loop top it snapshots the beam and
    /// frame counter; at the frame-skip nudge check it logs how far the beam
    /// advanced during the callback and whether the iteration crossed a vblank.
    fn debug_check_sched(&mut self, pc: u32) {
        if !self.dbg_sched_on {
            return;
        }
        const LOOP_TOP: u32 = 0x00C0_3374;
        const NUDGE: u32 = 0x00C0_33BE;
        // 0xC035D2: the VERTB server's `addq.l #1,$C09580` -- the instant the
        // frame counter increments. Logging its beam position tells us the
        // vblank->loop-top startup latency that eats the frame budget.
        const COUNTER_INC: u32 = 0x00C0_35D2;
        // 0xC0339C: instruction right after `jsr (a1)` returns -- the render
        // callback is done. 0xC033B8: after the SET SOFT yield to task B has
        // returned. Together they split the per-frame window into render time and
        // task-B time.
        const RENDER_DONE: u32 = 0x00C0_339C;
        const SOFTINT_RET: u32 = 0x00C0_33B8;
        // 0xC03702: the RTE that completes the cooperative switch INTO task B
        // (the loader). The top of the freshly-loaded task-B stack holds SR then
        // the loader's resume PC -- logging it reveals the loader's main loop /
        // yield point, which sizes each decrunch slice.
        const SWITCH_B_RTE: u32 = 0x00C0_3702;
        let is_bp = pc == LOOP_TOP
            || pc == NUDGE
            || pc == COUNTER_INC
            || pc == RENDER_DONE
            || pc == SOFTINT_RET
            || pc == SWITCH_B_RTE;
        // Exception entries surface here at the handler's first instruction
        // (the dispatch already pushed [SR][return PC] at a7).
        let vec_entry = self.cpu.last_exception_vector.take();
        let in_flight = self.dbg_sched_vec_return_pc.iter().any(|&p| p != 0);
        // Fast path: when not capturing the task-B span nor timing an
        // interrupt handler, only the five breakpoint PCs and exception
        // entries matter.
        if !self.dbg_sched_capturing && !is_bp && vec_entry.is_none() && !in_flight {
            return;
        }
        let secs = self.bus.bus.emulated_seconds();
        let after = crate::envcfg::var("COPPERLINE_DBG_AFTER")
            .and_then(|s| s.trim().parse::<f64>().ok())
            .unwrap_or(0.0);
        let until = crate::envcfg::var("COPPERLINE_DBG_UNTIL")
            .and_then(|s| s.trim().parse::<f64>().ok())
            .unwrap_or(f64::INFINITY);
        if secs < after || secs >= until {
            return;
        }
        if let Some(vector) = vec_entry {
            let sp = self.cpu.a(7);
            let return_pc = (u32::from(self.bus.bus.peek_word_any(sp.wrapping_add(2))) << 16)
                | u32::from(self.bus.bus.peek_word_any(sp.wrapping_add(4)));
            log::info!(
                "sched vec={} secs={secs:.5} v={} h={} return_pc={return_pc:#010X}",
                vector,
                self.bus.bus.agnus.vpos,
                self.bus.bus.agnus.hpos,
            );
            // Autovectors 25..31 = IPL levels 1..7.
            if (25..=31).contains(&vector) {
                let level = (vector - 24) as usize;
                self.dbg_sched_vec_return_pc[level] = return_pc;
                self.dbg_sched_vec_entry_cck[level] = self.bus.bus.emulated_cck();
                self.dbg_sched_vec_entry_vpos[level] = self.bus.bus.agnus.vpos;
                self.dbg_sched_vec_entry_hpos[level] = self.bus.bus.agnus.hpos;
                if level == 6 {
                    self.dbg_sched_vec30_prev_pc = pc;
                    self.dbg_sched_vec30_prev_cck = self.dbg_sched_vec_entry_cck[level];
                }
            }
        } else if in_flight {
            let now_cck = self.bus.bus.emulated_cck();
            if self.dbg_sched_vec_return_pc[6] != 0 {
                // Bill the level-6 instruction that just retired (prev_pc)
                // with the cck the bus advanced since it started;
                // slice-boundary chipset catch-up rides on whichever
                // instruction spans it.
                let cost = self
                    .dbg_sched_vec30_cost
                    .entry(self.dbg_sched_vec30_prev_pc)
                    .or_insert((0, 0));
                cost.0 += 1;
                cost.1 += now_cck.saturating_sub(self.dbg_sched_vec30_prev_cck);
                if pc != self.dbg_sched_vec_return_pc[6] {
                    self.dbg_sched_vec30_prev_pc = pc;
                    self.dbg_sched_vec30_prev_cck = now_cck;
                }
            }
            for level in 1..8usize {
                if self.dbg_sched_vec_return_pc[level] == 0
                    || pc != self.dbg_sched_vec_return_pc[level]
                {
                    continue;
                }
                self.dbg_sched_vec_return_pc[level] = 0;
                log::info!(
                    "sched vecdur level={level} secs={secs:.5} cck={} entry(v={} h={}) exit(v={} h={})",
                    now_cck.saturating_sub(self.dbg_sched_vec_entry_cck[level]),
                    self.dbg_sched_vec_entry_vpos[level],
                    self.dbg_sched_vec_entry_hpos[level],
                    self.bus.bus.agnus.vpos,
                    self.bus.bus.agnus.hpos,
                );
                if level != 6 {
                    continue;
                }
                self.dbg_sched_vec30_runs += 1;
                if self.dbg_sched_vec30_runs == 40 && !self.dbg_sched_vec30_cost_dumped {
                    self.dbg_sched_vec30_cost_dumped = true;
                    let mut rows: Vec<(u32, u64, u64)> = self
                        .dbg_sched_vec30_cost
                        .iter()
                        .map(|(&p, &(n, cck))| (p, n, cck))
                        .collect();
                    rows.sort_by_key(|r| r.0);
                    log::info!("sched vec30-cost over {} runs:", self.dbg_sched_vec30_runs);
                    for (p, n, cck) in rows {
                        log::info!(
                            "sched vec30-cost PC={p:#010X} op={:04X} n={n} avg_cck={:.2}",
                            self.bus.bus.peek_word_any(p),
                            cck as f64 / n as f64,
                        );
                    }
                }
            }
        }
        // While inside the render-done -> task-B-return span, histogram every PC
        // so we can see what task B (and the interrupt handlers that fire in the
        // window) actually execute.
        if self.dbg_sched_capturing {
            *self.dbg_sched_bhist.entry(pc).or_insert(0) += 1;
        }
        if !is_bp {
            return;
        }
        // $C09580 is the VERTB-incremented frame counter watched by this diagnostic.
        let counter = self.bus.peek_word(0x00C0_9582); // low word of the long
        let vpos = self.bus.bus.agnus.vpos;
        let hpos = self.bus.bus.agnus.hpos;
        let frame = self.bus.bus.emulated_frames();
        if pc == COUNTER_INC {
            log::info!("sched vertb-inc secs={secs:.5} f={frame} v={vpos} h={hpos}");
            return;
        }
        if pc == SWITCH_B_RTE {
            // a7 -> [SR][resume PC]; the loader resumes at the longword at a7+2.
            // The loader's registers are live here (the switch restored them), so
            // a1/a3 are the decrunch dest/source: their per-frame deltas measure
            // how much the loader decrunches per beam-bounded slice.
            let sp = self.cpu.a(7);
            let hi = self.bus.bus.peek_word_any(sp.wrapping_add(2));
            let lo = self.bus.bus.peek_word_any(sp.wrapping_add(4));
            let resume = (u32::from(hi) << 16) | u32::from(lo);
            log::info!(
                "sched loader-resume secs={secs:.5} pc={resume:#010X} dest={:#010X} src={:#010X} end={:#010X}",
                self.cpu.a(1),
                self.cpu.a(3),
                self.cpu.a(2),
            );
            return;
        }
        if pc == LOOP_TOP {
            if !self.dbg_sched_loop_dumped {
                self.dbg_sched_loop_dumped = true;
                let mut words = String::new();
                for i in 0..56u32 {
                    let a = LOOP_TOP.wrapping_add(i * 2);
                    words.push_str(&format!(" {:04X}", self.bus.bus.peek_word_any(a)));
                }
                log::info!("sched loop-code {LOOP_TOP:#010X}..:{words}");
                let mut words = String::new();
                for i in 0..72u32 {
                    let a = 0x00C0_3580u32.wrapping_add(i * 2);
                    words.push_str(&format!(" {:04X}", self.bus.bus.peek_word_any(a)));
                }
                log::info!("sched vertb-code 0x00C03580..:{words}");
            }
            self.dbg_sched_top_vpos = vpos;
            self.dbg_sched_top_frame = frame;
            self.dbg_sched_top_counter = counter;
            return;
        }
        if pc == RENDER_DONE {
            self.dbg_sched_render_vpos = vpos;
            self.dbg_sched_render_frame = frame;
            // Start histogramming the task-B (depacker) window: render-done -> b8.
            self.dbg_sched_capturing = true;
            return;
        }
        if pc == SOFTINT_RET {
            self.dbg_sched_b8_vpos = vpos;
            self.dbg_sched_b8_frame = frame;
            self.dbg_sched_capturing = false;
            if !self.dbg_sched_bhist_dumped && !self.dbg_sched_bhist.is_empty() {
                self.dbg_sched_bhist_dumped = true;
                let total: u64 = self.dbg_sched_bhist.values().sum();
                let mut top: Vec<(u32, u64)> =
                    self.dbg_sched_bhist.iter().map(|(&p, &n)| (p, n)).collect();
                top.sort_by_key(|b| std::cmp::Reverse(b.1));
                log::info!("sched taskB-window total={total} unique_pc={}", top.len());
                for (p, n) in top.iter().take(25) {
                    let w0 = self.bus.bus.peek_word_any(*p);
                    let w1 = self.bus.bus.peek_word_any(p.wrapping_add(2));
                    let pct = *n as f64 / total as f64 * 100.0;
                    log::info!(
                        "sched taskB PC={p:#010X} n={n:6} ({pct:5.2}%) words={w0:04X} {w1:04X}"
                    );
                }
            }
            return;
        }
        // pc == NUDGE: report the iteration span and its render / task-B split,
        // plus the pacing-gate state: the live frame counter, d2 (the target
        // the wait loop compares against), and the scene descriptor's step.
        let frame_delta = frame.wrapping_sub(self.dbg_sched_top_frame);
        let counter_delta = counter.wrapping_sub(self.dbg_sched_top_counter);
        let peek_long = |bus: &crate::bus::Bus, addr: u32| -> u32 {
            (u32::from(bus.peek_word_any(addr)) << 16)
                | u32::from(bus.peek_word_any(addr.wrapping_add(2)))
        };
        let counter_live = peek_long(&self.bus.bus, 0x00C0_9580);
        let descriptor = peek_long(&self.bus.bus, 0x00C0_9578);
        let step = if descriptor != 0 {
            peek_long(&self.bus.bus, descriptor)
        } else {
            0
        };
        log::info!(
            "sched iter secs={secs:.5} top(f={} v={}) render(f={} v={}) b8(f={} v={}) \
             nudge(f={} v={}) frame_delta={} counter_delta={} crossed_vblank={} d2={:#X} \
             counter={counter_live:#X} step={step} desc={descriptor:#010X}",
            self.dbg_sched_top_frame,
            self.dbg_sched_top_vpos,
            self.dbg_sched_render_frame,
            self.dbg_sched_render_vpos,
            self.dbg_sched_b8_frame,
            self.dbg_sched_b8_vpos,
            frame,
            vpos,
            frame_delta,
            counter_delta,
            frame_delta > 0,
            self.cpu.d(2),
        );
    }

    fn debug_check_spren_clear(&mut self) {
        if !self.dbg_spren_on {
            return;
        }
        const SPREN: u16 = 0x0020;
        let dmacon = self.bus.bus.debug_dmacon();
        let prev = self.dbg_prev_dmacon;
        self.dbg_prev_dmacon = dmacon;
        if prev & SPREN != 0 && dmacon & SPREN == 0 {
            let secs = self.bus.bus.emulated_seconds();
            let after = crate::envcfg::var("COPPERLINE_DBG_AFTER")
                .and_then(|s| s.trim().parse::<f64>().ok())
                .unwrap_or(0.0);
            let until = crate::envcfg::var("COPPERLINE_DBG_UNTIL")
                .and_then(|s| s.trim().parse::<f64>().ok())
                .unwrap_or(f64::INFINITY);
            if secs >= after && secs < until {
                self.debug_report(
                    &format!(
                        "SPREN-CLEAR dmacon {prev:#06X}->{dmacon:#06X} writer_pc={:#010X}",
                        self.cpu.ppc
                    ),
                    secs,
                );
            }
        }
    }

    fn debug_after_step(&mut self, snapshot: Vec<(u32, u16)>) {
        let secs = self.bus.bus.emulated_seconds();
        for (addr, old) in snapshot {
            let new = self.bus.bus.peek_word_any(addr);
            if new != old {
                // ppc is the PC of the instruction that just executed (and
                // therefore did the write).
                let writer = self.cpu.ppc;
                self.debug_report(
                    &format!("WATCH {addr:#010X} {old:04X}->{new:04X} by pc={writer:#010X}"),
                    secs,
                );
            }
        }
        self.debug_check_exec_lists(secs);
    }

    /// COPPERLINE_DBG_LISTCHECK: after each instruction in the window, walk
    /// the configured exec List headers and report the first instruction
    /// after which a node is linked twice (within one list or across the
    /// listed lists) or a chain no longer terminates.
    fn debug_check_exec_lists(&mut self, secs: f64) {
        const NODE_LIMIT: usize = 4096;
        let headers: Vec<u32> = match self.dbg.as_ref() {
            Some(d) if !d.listcheck.is_empty() && !d.listcheck_reported => d.listcheck.clone(),
            _ => return,
        };
        let violation = crate::debugger::find_exec_list_violation(&headers, NODE_LIMIT, |addr| {
            (u32::from(self.bus.bus.peek_word_any(addr)) << 16)
                | u32::from(self.bus.bus.peek_word_any(addr.wrapping_add(2)))
        });
        if let Some(violation) = violation {
            let text = match violation {
                crate::debugger::ExecListViolation::Duplicate {
                    node,
                    first_head,
                    second_head,
                } => format!(
                    "LISTCHECK node {node:#010X} linked twice (lists {first_head:#010X} and {second_head:#010X}) by pc={:#010X}",
                    self.cpu.ppc
                ),
                crate::debugger::ExecListViolation::Unterminated { head, limit } => format!(
                    "LISTCHECK list {head:#010X} unterminated (>{limit} nodes) by pc={:#010X}",
                    self.cpu.ppc
                ),
            };
            self.debug_report(&text, secs);
            if let Some(d) = self.dbg.as_mut() {
                d.listcheck_reported = true;
            }
        }
    }

    /// Emit a debugger hit: the event line, full register set, configured
    /// memory dumps, and (when COPPERLINE_DBG_SHOT is set) a screenshot of the
    /// current frame. Counts toward the hit budget.
    fn debug_report(&mut self, detail: &str, secs: f64) {
        let frame = self.bus.bus.emulated_frames();
        log::info!(
            "DBG {detail} t={secs:.5} f={frame} v={} h={} sr={:#06X} pc={:#010X}",
            self.bus.bus.agnus.vpos,
            self.bus.bus.agnus.hpos,
            self.cpu.get_sr(),
            self.cpu.pc,
        );
        for reg in 0..8 {
            log::info!(
                "  D{reg}={:#010X}  A{reg}={:#010X}",
                self.cpu.d(reg),
                self.cpu.a(reg)
            );
        }
        log::info!("  {}", self.bus.bus.debug_display_state());
        let dumps: Vec<(u32, u32)> = self
            .dbg
            .as_ref()
            .map(|d| d.dumps.clone())
            .unwrap_or_default();
        for (addr, words) in dumps {
            let mut row = String::new();
            for k in 0..words {
                row.push_str(&format!(
                    "{:04X} ",
                    self.bus.bus.peek_word_any(addr + k * 2)
                ));
            }
            log::info!("  mem {addr:#010X}: {row}");
        }
        self.debug_screenshot();
        if let Some(d) = self.dbg.as_mut() {
            d.hits += 1;
        }
    }

    /// Render the current (last completed) frame and save it, if COPPERLINE_DBG_SHOT
    /// is configured. Used to capture exactly what is on screen at a hit.
    fn debug_screenshot(&mut self) {
        let Some(path) = self.dbg.as_mut().and_then(|d| d.next_shot_path()) else {
            return;
        };
        let mut fb = vec![0u32; crate::video::MAX_CANVAS_PIXELS];
        crate::video::bitplane::render(&mut self.bus.bus, &mut fb);
        let geometry = self.bus.bus.frame_geometry();
        let canvas_scale = self.bus.bus.frame_canvas_scale();
        let canvas_width = crate::video::FB_WIDTH * canvas_scale;
        let mut present_rows = geometry.visible_lines;
        if !geometry.programmable {
            let visible_start = self.bus.bus.frame_visible_start_vpos();
            crate::video::present_common::center_present_frame_for_visible_start(
                &mut fb,
                visible_start,
            );
        } else {
            if let Some((src_x0, src_w)) = self.bus.bus.frame_presentation_h_window() {
                crate::screenshot::stretch_rows_x_window(
                    &mut fb,
                    canvas_width,
                    geometry.visible_lines,
                    src_x0 * canvas_scale as i32,
                    src_w * canvas_scale as u32,
                );
            } else if geometry.line_cck != 227 {
                crate::screenshot::stretch_rows_x(
                    &mut fb,
                    canvas_width,
                    geometry.visible_lines,
                    geometry.line_cck,
                    227,
                );
            }
            present_rows = crate::video::present_common::apply_presentation_v_window(
                &mut fb,
                canvas_width,
                geometry.visible_lines,
                self.bus.bus.frame_presentation_v_window(),
            );
        }
        match crate::screenshot::save_scaled_y(
            std::path::Path::new(&path),
            &fb,
            canvas_width as u32,
            present_rows as u32,
            (crate::video::capture_height() * canvas_scale) as u32,
        ) {
            Ok(()) => log::info!("  screenshot: {path}"),
            Err(e) => log::warn!("  screenshot failed ({path}): {e:#}"),
        }
    }

    pub fn bus(&self) -> &Bus {
        &self.bus.bus
    }

    pub fn bus_mut(&mut self) -> &mut Bus {
        &mut self.bus.bus
    }

    fn runtime_state(&self) -> MachineRuntimeState {
        MachineRuntimeState {
            last_cacr: self.last_cacr,
            sync_cck_on: self.sync_cck_on,
            cpu_clocks_per_cck: self.cpu_clocks_per_cck,
            cpu_clock_carry: self.cpu_clock_carry,
        }
    }

    /// Serialize the machine's emulated state (CPU core, timing carries,
    /// cache models, Bus) into `w` as positional bincode, in the fixed
    /// component order `apply_state` reads back. This is the in-process
    /// snapshot form (reverse debugging, run-ahead, netplay rollback): fast,
    /// unframed, and only ever read by the build that wrote it. Files use
    /// `write_chunks`. Host-side state (debugger instrumentation, sinks,
    /// trace files) is not written. Call only at an emulated-frame boundary;
    /// mid-frame the renderer capture buffers are inconsistent.
    pub(crate) fn write_state<W: std::io::Write>(&self, w: &mut W) -> Result<()> {
        serialize_component(w, &self.cpu, "CPU core")?;
        serialize_component(w, &self.runtime_state(), "machine runtime")?;
        serialize_component(w, &self.bus.icache, "icache")?;
        serialize_component(w, &self.bus.dcache, "dcache")?;
        serialize_component(w, &self.bus.bus, "bus")?;
        Ok(())
    }

    /// The file-format counterpart of `write_state`: the same components as
    /// tagged, versioned chunks, with the `Bus` split by subsystem
    /// (`savestate::chunk` lists them). Same snapshot-point rules.
    pub(crate) fn write_chunks<W: std::io::Write>(
        &self,
        out: &mut chunk::ChunkWriter<W>,
    ) -> Result<()> {
        out.value(&chunk::CPU, &self.cpu)?;
        out.value(&chunk::MACH, &self.runtime_state())?;
        out.value(&chunk::ICAC, &self.bus.icache)?;
        out.value(&chunk::DCAC, &self.bus.dcache)?;
        let parts = split::BusSplitter::split(&self.bus.bus)
            .map_err(|e| anyhow!("serializing bus: {e}"))?;
        for (spec, payload) in &parts {
            out.write(spec, payload)?;
        }
        Ok(())
    }

    /// Counterpart of `write_chunks`: decode every component (upgrading
    /// chunks written at older versions through `migrations`), then swap the
    /// machine onto the restored state exactly as `apply_state` does. The
    /// live machine is untouched if any chunk fails to decode.
    pub(crate) fn apply_chunks(
        &mut self,
        chunks: &chunk::ChunkSet,
        migrations: &[chunk::Migration],
    ) -> Result<()> {
        let cpu = chunks.value(&chunk::CPU, migrations)?;
        let runtime = chunks.value(&chunk::MACH, migrations)?;
        let icache = chunks.value(&chunk::ICAC, migrations)?;
        let dcache = chunks.value(&chunk::DCAC, migrations)?;
        let parts = chunks.bus_parts(migrations)?;
        let bus = serde::Deserialize::deserialize(split::BusJoiner::new(&parts)).map_err(|e| {
            // serde reports a required field a chunk lacks as "missing field
            // `name`"; say which chunk should have carried it.
            let text = e.to_string();
            match text
                .strip_prefix("missing field `")
                .and_then(|rest| rest.strip_suffix('`'))
            {
                Some(field) => {
                    let spec = chunk::chunk_for_field(field);
                    anyhow!(
                        "the {} chunk ({}) has no `{field}` field",
                        chunk::tag_name(spec.tag),
                        spec.name
                    )
                }
                None => anyhow!("restoring the bus from its chunks: {text}"),
            }
        })?;
        self.adopt_state(
            RestoredState {
                cpu,
                runtime,
                icache,
                dcache,
                bus,
            },
            None,
        )
    }

    pub(crate) fn write_rollback_state<W: std::io::Write>(&self, w: &mut W) -> Result<()> {
        let runtime = MachineRollbackState {
            bus: self.bus.bus.rollback_state(),
            sampled_irq_level: self.bus.sampled_irq_level,
            ipl_sample_held: self.bus.ipl_sample_held,
        };
        serialize_component(w, &runtime, "rollback latches")?;
        self.write_state(w)
    }

    pub(crate) fn apply_rollback_state<R: std::io::Read>(&mut self, r: &mut R) -> Result<()> {
        let runtime = deserialize_component(r, "rollback latches")?;
        self.apply_state_inner(r, Some(runtime))
    }

    /// Counterpart of `write_state`: parse every component from `r`, then
    /// swap the machine onto the restored state. The live machine is left
    /// untouched if any component fails to parse. Host resources (audio and
    /// serial sinks, blitter trace file) move across to the restored Bus;
    /// debugger state and breakpoints stay live.
    pub(crate) fn apply_state<R: std::io::Read>(&mut self, r: &mut R) -> Result<()> {
        self.apply_state_inner(r, None)
    }

    fn apply_state_inner<R: std::io::Read>(
        &mut self,
        r: &mut R,
        rollback: Option<MachineRollbackState>,
    ) -> Result<()> {
        let state = RestoredState {
            cpu: deserialize_component(r, "CPU core")?,
            runtime: deserialize_component(r, "machine runtime")?,
            icache: deserialize_component(r, "icache")?,
            dcache: deserialize_component(r, "dcache")?,
            bus: deserialize_component(r, "bus")?,
        };
        self.adopt_state(state, rollback)
    }

    /// Swap the machine onto a fully parsed state. Host resources (audio
    /// and serial sinks, blitter trace file) move across to the restored
    /// Bus; debugger state and breakpoints stay live.
    fn adopt_state(
        &mut self,
        state: RestoredState,
        rollback: Option<MachineRollbackState>,
    ) -> Result<()> {
        let RestoredState {
            cpu,
            runtime,
            icache,
            dcache,
            mut bus,
        } = state;

        bus.adopt_host_resources(&mut self.bus.bus)?;
        bus.adopt_ui_debug_state(&mut self.bus.bus);
        if let Some(runtime) = rollback {
            bus.restore_rollback_state(runtime.bus);
            self.bus.sampled_irq_level = runtime.sampled_irq_level;
            self.bus.ipl_sample_held = runtime.ipl_sample_held;
        } else {
            bus.reset_transient_video_after_state_load();
        }
        bus.reset_transient_diagnostics_after_state_load();
        // The CPU model travels with the state (cpu_type, timing tables, and
        // address_mask all live in CpuCore); keep the bus adapter's mask copy
        // in step so external decode agrees with the restored core.
        self.bus.address_mask = cpu.address_mask;
        self.cpu = cpu;
        self.bus.bus = bus;
        // The cache is silicon, not dynamic state: a CPU that has one keeps it
        // across a restore. Snapshots taken with the cache modelled carry warm
        // line contents (used as-is for byte-identical replay); ones that
        // predate cache modelling - or were captured with it disabled - have
        // None, so we keep the live machine's (cold) cache rather than dropping
        // it, then re-derive its enable/freeze flags from the restored CACR.
        self.bus.icache = icache.or_else(|| self.bus.icache.take());
        self.bus.dcache = dcache.or_else(|| self.bus.dcache.take());
        self.last_cacr = runtime.last_cacr;
        self.sync_cck_on = runtime.sync_cck_on;
        self.cpu_clocks_per_cck = runtime.cpu_clocks_per_cck;
        self.cpu_clock_carry = runtime.cpu_clock_carry;
        // The short-bus-cycle flag is derived from the (restored) CPU model and
        // not serialized, so re-establish it here.
        let short_bus = !self.cpu.is_pre_68020;
        self.bus.bus.set_cpu_short_bus_cycle(short_bus);
        // apply_state runs outside the per-instruction loop that pushes CACR
        // into the cache models, so force a re-sync now: a cache kept cold from
        // above still holds power-on (disabled) flags until CACR is applied.
        if self.bus.icache.is_some() || self.bus.dcache.is_some() {
            self.last_cacr = !self.cpu.cacr;
            self.apply_cacr_updates();
        }
        // Re-baseline the interactive stop conditions on the restored
        // timeline: watch compare values and the scheduled-task pointer
        // describe state, so carrying pre-restore baselines over would
        // fire phantom hits on the first step after a restore.
        self.ui_rebaseline_watches();
        self.ui_last_this_task = self.ui_peek_this_task();
        let addrs: Vec<u32> = self.ui_breaks.watches.iter().map(|w| w.addr).collect();
        self.bus.bus.set_ui_mem_watches(&addrs);
        Ok(())
    }

    pub fn pc(&self) -> u32 {
        self.cpu.pc
    }

    /// PC of the most recently retired instruction (the "previous PC").
    /// Reverse debugging credits a watched-memory change to this PC, matching
    /// the `COPPERLINE_DBG_WATCH` writer attribution.
    pub fn ppc(&self) -> u32 {
        self.cpu.ppc
    }

    pub fn sr(&self) -> u16 {
        self.cpu.get_sr()
    }

    /// The vector base register (always 0 on a 68000).
    pub fn vbr(&self) -> u32 {
        self.cpu.vbr
    }

    /// Press the freezer cartridge's button: the bus snapshots the
    /// register shadows, points the level-7 vector under the current VBR
    /// at the monitor and raises the non-maskable interrupt, taken at the
    /// next instruction boundary. Returns the monitor's entry address.
    pub fn cartridge_freeze(&mut self) -> Result<u32, crate::cartridge::FreezeError> {
        let vbr = self.cpu.vbr;
        self.bus.bus.cartridge_freeze(vbr)
    }

    pub fn a(&self, reg: usize) -> u32 {
        self.cpu.a(reg)
    }

    /// The user stack pointer: A7 in user mode, the banked USP while the
    /// CPU is in supervisor mode. AmigaOS tasks run in user mode, so this
    /// is the running task's own stack pointer even when the snapshot
    /// lands inside an interrupt.
    pub fn usp(&self) -> u32 {
        self.cpu.get_usp()
    }

    pub fn d(&self, reg: usize) -> u32 {
        self.cpu.d(reg)
    }

    /// Return one GDB-style core register: D0-D7, A0-A7, SR, PC.
    pub fn debug_register(&self, reg: usize) -> Option<u32> {
        match reg {
            0..=7 => Some(self.cpu.d(reg)),
            8..=15 => Some(self.cpu.a(reg - 8)),
            16 => Some(u32::from(self.cpu.get_sr())),
            17 => Some(self.cpu.pc),
            _ => None,
        }
    }

    /// Raw 80-bit FPU registers and the three control registers, when an
    /// FPU is fitted. The `(sign+exponent, mantissa)` form is exact and
    /// stable for debugger protocols.
    pub fn debug_fpu_registers(&self) -> Option<DebugFpuRegisters> {
        self.fpu_enabled.then(|| {
            (
                self.cpu.fpr.map(|f| (f.sign_exp, f.mantissa)),
                self.cpu.fpcr,
                self.cpu.fpsr,
                self.cpu.fpiar,
            )
        })
    }

    /// Restore the architectural CPU state from a WinUAE save state.
    pub(crate) fn import_uss_cpu(&mut self, state: &crate::uss::CpuState) {
        for (reg, &value) in state.registers.iter().enumerate() {
            self.cpu.dar[reg] = value;
        }
        self.cpu.set_sr(state.sr);
        self.cpu.write_control_register(0x800, state.usp);
        self.cpu.write_control_register(0x804, state.isp);
        self.cpu.write_control_register(0x803, state.msp);
        self.cpu.dfc = state.dfc;
        self.cpu.sfc = state.sfc;
        self.cpu.vbr = state.vbr;
        self.cpu.caar = state.caar;
        self.cpu.cacr = state.cacr;
        self.cpu.jump(state.pc);
        self.cpu.invalidate_prefetch();
        self.cpu.stopped = u32::from(state.stopped);
        self.cpu_clock_carry = 0;
    }

    /// Set one GDB-style core register: D0-D7, A0-A7, SR, PC.
    pub fn debug_set_register(&mut self, reg: usize, value: u32) -> bool {
        match reg {
            0..=7 => self.cpu.set_d(reg, value),
            8..=15 => self.cpu.set_a(reg - 8, value),
            16 => self.cpu.set_sr(value as u16),
            17 => {
                self.cpu.pc = value & self.bus.address_mask;
                self.cpu.invalidate_prefetch();
            }
            _ => return false,
        }
        true
    }

    /// Side-effect-free CPU-visible memory read for remote debuggers. This
    /// respects the current address width and boot ROM overlay, but does not
    /// access device registers or charge bus time.
    pub fn debug_read_memory(&self, addr: u32, len: usize) -> Vec<u8> {
        (0..len)
            .map(|idx| self.bus.peek_byte(addr.wrapping_add(idx as u32)))
            .collect()
    }

    /// Side-effect-free CPU-visible memory write for remote debuggers. Only
    /// RAM-backed regions are mutated; ROM, overlay ROM, and device windows are
    /// ignored. Returns the number of bytes actually written.
    pub fn debug_write_memory(&mut self, addr: u32, bytes: &[u8]) -> usize {
        let mut written = 0usize;
        for (idx, byte) in bytes.iter().enumerate() {
            if self
                .bus
                .debug_write_byte(addr.wrapping_add(idx as u32), *byte)
            {
                written += 1;
            }
        }
        written
    }

    pub fn cpu_type(&self) -> CpuType {
        self.cpu.cpu_type
    }

    /// CPU clocks per colour clock for the running machine. Restored by
    /// `apply_state`, so the host pacing math can be re-derived after a load
    /// that swaps in a differently-clocked CPU.
    pub fn cpu_clocks_per_cck(&self) -> u32 {
        self.cpu_clocks_per_cck
    }

    #[cfg(feature = "control")]
    fn profile_registers(&self) -> [u32; crate::profile::samples::REGISTER_COUNT] {
        let mut registers = [0; crate::profile::samples::REGISTER_COUNT];
        for reg in 0..8 {
            registers[reg] = self.cpu.d(reg);
            registers[reg + 8] = self.cpu.a(reg);
        }
        registers[16] = u32::from(self.cpu.get_sr());
        registers
    }

    #[cfg(feature = "control")]
    fn profile_sample_start(
        &self,
        pc: u32,
        bus_wait_cck: u64,
    ) -> Option<crate::profile::samples::SampleStart> {
        self.profile_samples.as_ref().map(|sampler| {
            sampler.start(
                pc & self.cpu.address_mask,
                self.profile_registers(),
                bus_wait_cck,
                self.bus.bus.slice_bus_advanced_cck(),
            )
        })
    }

    #[cfg(feature = "control")]
    fn profile_finish_instruction_sample(
        &mut self,
        start: crate::profile::samples::SampleStart,
        instruction_cck: u32,
    ) {
        if let Some(sampler) = self.profile_samples.as_mut() {
            sampler.finish_instruction(start, instruction_cck, &self.bus.bus);
        }
    }

    #[cfg(feature = "control")]
    fn profile_finish_irq_sample(
        &mut self,
        start: crate::profile::samples::SampleStart,
        instruction_cck: u32,
        level: u8,
        vector: u16,
    ) {
        if let Some(sampler) = self.profile_samples.as_mut() {
            sampler.finish_irq(
                start,
                instruction_cck,
                crate::profile::samples::IrqInfo { level, vector },
                &self.bus.bus,
            );
        }
    }

    #[cfg(feature = "control")]
    pub fn start_profile_samples(
        &mut self,
        unwind: Option<crate::profile::samples::CompactUnwindTable>,
        code_ranges: Vec<(u32, u32)>,
        registers: bool,
    ) {
        self.profile_samples = Some(crate::profile::samples::InstructionSampler::new(
            unwind,
            code_ranges,
            registers,
        ));
        self.note_jit_debug_fallback();
    }

    #[cfg(feature = "control")]
    pub fn stop_profile_samples(&mut self) {
        self.profile_samples = None;
    }

    #[cfg(feature = "control")]
    pub fn take_profile_samples(&mut self) -> Vec<crate::profile::samples::InstructionSample> {
        self.profile_samples
            .as_mut()
            .map(crate::profile::samples::InstructionSampler::take)
            .unwrap_or_default()
    }

    #[cfg(feature = "control")]
    pub fn discard_profile_samples(&mut self) {
        if let Some(sampler) = self.profile_samples.as_mut() {
            sampler.clear();
        }
    }

    /// Whether the CPU is halted in STOP waiting for an interrupt.
    pub fn stopped(&self) -> bool {
        self.cpu.stopped != 0
    }

    /// The debugger window's breakpoint/watchpoint set (read-only view).
    /// Enable/disable the recent-PC ring (cleared on enable). Recording
    /// costs one array write per retired instruction, so it only runs
    /// while a debug window has it on.
    pub fn ui_set_pc_history_enabled(&mut self, enabled: bool) {
        if enabled && !self.ui_pc_history_enabled {
            self.ui_pc_history_len = 0;
            self.ui_pc_history_next = 0;
        }
        self.ui_pc_history_enabled = enabled;
    }

    /// The recent-PC ring, oldest first.
    pub fn ui_pc_history(&self) -> Vec<u32> {
        let mut out = Vec::with_capacity(self.ui_pc_history_len);
        let start = (self.ui_pc_history_next + UI_PC_HISTORY_CAP - self.ui_pc_history_len)
            % UI_PC_HISTORY_CAP;
        for i in 0..self.ui_pc_history_len {
            out.push(self.ui_pc_history[(start + i) % UI_PC_HISTORY_CAP]);
        }
        out
    }

    /// Reset every word watchpoint's compare value to the live memory
    /// contents (used after timeline jumps).
    pub fn ui_rebaseline_watches(&mut self) {
        for i in 0..self.ui_breaks.watches.len() {
            let addr = self.ui_breaks.watches[i].addr;
            self.ui_breaks.watches[i].last = self.bus.bus.peek_word_any(addr);
        }
    }

    /// Start a runtime instruction trace to `path`, replacing any
    /// running one. `cap` bounds the file (a final marker line notes a
    /// cap-stop). Costs a disassembly and a buffered write per retired
    /// instruction while active.
    pub fn ui_trace_start(&mut self, path: std::path::PathBuf, cap: u64) -> std::io::Result<()> {
        crate::paths::ensure_parent(&path)?;
        let file = std::fs::File::create(&path)?;
        self.ui_trace = Some(UiTrace {
            writer: std::io::BufWriter::new(file),
            path,
            lines: 0,
            cap: cap.max(1),
        });
        Ok(())
    }

    /// Stop the runtime trace, flushing it. Returns (path, lines).
    pub fn ui_trace_stop(&mut self) -> Option<(std::path::PathBuf, u64)> {
        use std::io::Write;
        let mut trace = self.ui_trace.take()?;
        let _ = trace.writer.flush();
        Some((trace.path, trace.lines))
    }

    /// The running trace's path and line count, if one is active.
    pub fn ui_trace_status(&self) -> Option<(&std::path::Path, u64)> {
        self.ui_trace
            .as_ref()
            .map(|trace| (trace.path.as_path(), trace.lines))
    }

    /// Arm a waveform (VCD) capture on the machine, replacing any
    /// existing one. See src/waveform.rs.
    pub fn ui_wave_start(&mut self, opts: crate::waveform::WaveOptions) -> std::io::Result<()> {
        self.bus.bus.wave_arm(opts)
    }

    /// Stop and discard the waveform capture, returning its final status.
    pub fn ui_wave_stop(&mut self) -> Option<crate::waveform::WaveStatus> {
        self.bus.bus.wave_stop()
    }

    /// Status of the waveform capture (armed/capturing/done), if any.
    pub fn ui_wave_status(&self) -> Option<crate::waveform::WaveStatus> {
        self.bus.bus.wave_status()
    }

    fn ui_trace_record(&mut self, pc: u32) {
        use std::io::Write;
        let (text, _) = crate::disasm::disassemble(
            |addr| self.bus.bus.peek_word_any(addr),
            pc,
            self.cpu_type(),
        );
        let vpos = self.bus.bus.agnus.vpos;
        let hpos = self.bus.bus.agnus.hpos;
        let Some(trace) = self.ui_trace.as_mut() else {
            return;
        };
        let _ = writeln!(trace.writer, "{pc:06X} [{vpos:3},{hpos:3}]  {text}");
        trace.lines += 1;
        if trace.lines >= trace.cap {
            let _ = writeln!(
                trace.writer,
                "-- trace stopped at the {} line cap --",
                trace.cap
            );
            let _ = trace.writer.flush();
            self.ui_trace = None;
        }
    }

    /// Whether the CPU halted on a double fault (a bus/address error
    /// during exception processing) -- the hardware condition behind a
    /// dead machine that software alerts cannot even report.
    pub fn cpu_double_faulted(&self) -> bool {
        self.cpu.is_halted()
    }

    /// Force the halted state (tests of the surfacing path only; on the
    /// plain 68000 model the double fault is reachable via MMU-fault
    /// recursion, which is heavy to stage in a unit test).
    #[cfg(test)]
    pub fn test_force_double_fault(&mut self) {
        self.cpu.stopped = 1;
        self.cpu.run_mode = 1; // RUN_MODE_BERR_AERR_RESET
    }

    pub fn ui_breaks(&self) -> &crate::debugger::InteractiveBreaks {
        &self.ui_breaks
    }

    /// The CPU model's address-bus mask, for debugger surfaces that
    /// normalize or display CPU addresses (A0-A23 on 24-bit models,
    /// full 32 bits on 020+).
    pub fn ui_addr_mask(&self) -> u32 {
        self.cpu.address_mask
    }

    /// Toggle a PC breakpoint carrying an optional condition and ignore count.
    /// Returns true when the breakpoint is now set.
    pub fn ui_set_breakpoint(
        &mut self,
        addr: u32,
        cond: Option<crate::debugger::BreakCond>,
        ignore: u32,
    ) -> bool {
        self.ui_breaks.toggle_breakpoint_full(addr, cond, ignore)
    }

    /// Evaluate the breakpoint gate at `pc` against live register/memory state.
    /// Disjoint field borrows let the breakpoint set update its hit counter
    /// while reading the CPU core and bus.
    fn ui_breakpoint_stops(&mut self, pc: u32) -> bool {
        let ctx = MachineBreakContext {
            cpu: &self.cpu,
            bus: &self.bus.bus,
        };
        self.ui_breaks.breakpoint_stops(pc, &ctx)
    }

    /// Toggle a word watchpoint at `addr` (word-aligned), baselining it on
    /// the current memory contents. Returns true when now set.
    pub fn ui_toggle_watch(&mut self, addr: u32) -> bool {
        self.ui_toggle_watch_filtered(addr, None)
    }

    /// Like [`Self::ui_toggle_watch`], stopping only on writes from
    /// `filter`'s source when set. The watched addresses are mirrored
    /// into the bus (and its DMA engines) for writer attribution.
    pub fn ui_toggle_watch_filtered(
        &mut self,
        addr: u32,
        filter: Option<crate::debugger::WatchSource>,
    ) -> bool {
        self.ui_toggle_watch_qualified(addr, filter, None)
    }

    /// A watch further qualified by the instruction that made the
    /// access: with `pc` set it stops only when that PC was executing.
    /// Watching a word a dozen routines poke is otherwise a matter of
    /// stopping repeatedly until the interesting caller comes round.
    pub fn ui_toggle_watch_qualified(
        &mut self,
        addr: u32,
        filter: Option<crate::debugger::WatchSource>,
        pc: Option<u32>,
    ) -> bool {
        self.ui_toggle_watch_access(addr, filter, pc, crate::debugger::WatchAccess::Write)
    }

    /// Toggle a word watch with an explicit read/write access class.
    pub fn ui_toggle_watch_access(
        &mut self,
        addr: u32,
        filter: Option<crate::debugger::WatchSource>,
        pc: Option<u32>,
        access: crate::debugger::WatchAccess,
    ) -> bool {
        let addr = addr & self.cpu.address_mask & !1;
        let current = self.bus.bus.peek_word_any(addr);
        // Even, like every instruction address: an odd qualifier could
        // never match a writer PC and the watch would never fire.
        let pc = pc.map(|pc| pc & self.cpu.address_mask & !1);
        let added = self
            .ui_breaks
            .toggle_watch(addr, current, filter, pc, access);
        let addrs: Vec<u32> = self.ui_breaks.watches.iter().map(|w| w.addr).collect();
        self.bus.bus.set_ui_mem_watches(&addrs);
        added
    }

    /// Toggle a custom chipset register write watch (a word offset into
    /// $DFF000; a full $DFFxxx address is normalized). Returns true when
    /// now set. The offsets are mirrored into the Bus, whose register
    /// write path records hits from every writer (CPU and Copper).
    pub fn ui_toggle_reg_watch(&mut self, off: u16) -> bool {
        let added = self.ui_breaks.toggle_reg_watch(off);
        self.bus.bus.set_ui_reg_watches(&self.ui_breaks.reg_watches);
        added
    }

    /// Arm or clear the scheduled-task catch: stop when exec's ThisTask
    /// changes to a task whose name contains `target` (matched
    /// case-insensitively). Arming baselines on the currently scheduled
    /// task so the catch fires on the next reschedule, not instantly.
    pub fn ui_set_task_catch(&mut self, target: Option<String>) {
        self.ui_last_this_task = self.ui_peek_this_task();
        self.ui_breaks
            .set_task_catch(target.map(|t| t.to_ascii_uppercase()));
    }

    /// Toggle the loadseg catch: stop when the guest OS loads a program
    /// (a new seglist appearing in the scheduled process; `name` limits
    /// it to that basename, case-insensitive). Returns true when now
    /// set. Arming resets the observer and baselines it immediately on
    /// the running task world, so programs already loaded never fire --
    /// and a program loaded by the very next instruction does.
    pub fn ui_toggle_loadseg_catch(&mut self, name: Option<String>) -> bool {
        self.ui_loadseg_tracker = crate::amigaos::LibraryTracker::default();
        if self.ui_breaks.loadseg_catch.is_some() {
            self.ui_breaks.set_loadseg_catch(None);
            false
        } else {
            let tracker = &mut self.ui_loadseg_tracker;
            crate::amigaos::with_bus_memory(&self.bus.bus, |os| tracker.arm(os));
            self.ui_breaks
                .set_loadseg_catch(Some(crate::debugger::LoadSegCatch { name }));
            true
        }
    }

    /// The programs the armed loadseg catch has seen loaded, oldest
    /// first (empty while no catch is armed).
    pub fn ui_loadseg_modules(&self) -> &[crate::amigaos::TrackedModule] {
        self.ui_loadseg_tracker.modules()
    }

    /// ExecBase->ThisTask via side-effect-free peeks, when plausible.
    fn ui_peek_this_task(&self) -> Option<u32> {
        let peek32 = |addr: u32| {
            (u32::from(self.bus.bus.peek_word_any(addr)) << 16)
                | u32::from(self.bus.bus.peek_word_any(addr.wrapping_add(2)))
        };
        let execbase = peek32(4);
        if !crate::amigaos::plausible_ptr(execbase) {
            return None;
        }
        let task = peek32(execbase.wrapping_add(0x114));
        crate::amigaos::plausible_ptr(task).then_some(task)
    }

    /// The name a task node points at, uppercased for matching.
    fn ui_task_name(&self, task: u32) -> String {
        let peek32 = |addr: u32| {
            (u32::from(self.bus.bus.peek_word_any(addr)) << 16)
                | u32::from(self.bus.bus.peek_word_any(addr.wrapping_add(2)))
        };
        let name_ptr = peek32(task.wrapping_add(10));
        // Names may live in ROM or at odd addresses (the ROM devices' node
        // names are Kickstart strings), so no plausible_ptr bound here;
        // unmapped pointers read as zero bytes and end the string at once.
        if name_ptr == 0 {
            return String::new();
        }
        let mut name = String::new();
        for i in 0..30u32 {
            let addr = name_ptr.wrapping_add(i);
            let word = self.bus.bus.peek_word_any(addr & !1);
            let byte = if addr & 1 == 0 {
                (word >> 8) as u8
            } else {
                word as u8
            };
            if byte == 0 {
                break;
            }
            name.push(if (0x20..0x7F).contains(&byte) {
                byte as char
            } else {
                '.'
            });
        }
        name
    }

    /// Fire the loadseg catch when the guest OS loads a matching
    /// program. The tracker's own scheduled-task/module probe is the
    /// cheap pre-filter -- a ThisTask-change cache would miss a
    /// same-task cli_Module swap, which is exactly how the shell runs a
    /// command -- so the per-instruction cost is a handful of
    /// side-effect-free peeks, paid only while a catch is armed.
    fn ui_check_loadseg_catch(&mut self) {
        let Some(catch) = &self.ui_breaks.loadseg_catch else {
            return;
        };
        let tracker = &mut self.ui_loadseg_tracker;
        let event = crate::amigaos::with_bus_memory(&self.bus.bus, |os| {
            if !tracker.armed() {
                // Defensive re-baseline for any path that reset the
                // observer while the catch stayed set; the normal arm
                // happens in ui_toggle_loadseg_catch, before the next
                // instruction can install a module unobserved.
                tracker.arm(os);
            }
            tracker.observe(os).map(|module| {
                (
                    module.name.clone(),
                    module.segments.first().map_or(0, |seg| seg.start),
                )
            })
        });
        let Some((name, addr)) = event else { return };
        let matched = catch
            .name
            .as_deref()
            .is_none_or(|target| name.eq_ignore_ascii_case(target));
        if matched {
            self.ui_stop = Some(crate::debugger::DebugStop::LoadSeg { name, addr });
        }
    }

    /// Fire the task catch when exec reschedules to a matching task.
    fn ui_check_task_catch(&mut self) {
        if self.ui_breaks.task_catch.is_none() {
            return;
        }
        let Some(task) = self.ui_peek_this_task() else {
            return;
        };
        if self.ui_last_this_task == Some(task) {
            return;
        }
        self.ui_last_this_task = Some(task);
        let name = self.ui_task_name(task);
        let matched = self
            .ui_breaks
            .task_catch
            .as_deref()
            .is_some_and(|target| name.to_ascii_uppercase().contains(target));
        if matched {
            self.ui_stop = Some(crate::debugger::DebugStop::Task { name, addr: task });
        }
    }

    /// Toggle an exception catchpoint (stop when the CPU enters the
    /// vector). Returns true when now set. Arming clears any stale
    /// recorded exception so an old entry cannot fire immediately.
    pub fn ui_toggle_catch(&mut self, vector: u16) -> bool {
        let added = self.ui_breaks.toggle_catch(vector);
        if added {
            self.cpu.last_exception_vector = None;
        }
        added
    }

    pub fn ui_breaks_clear(&mut self) {
        self.ui_breaks.clear();
        self.ui_loadseg_tracker = crate::amigaos::LibraryTracker::default();
        self.bus.bus.set_ui_reg_watches(&[]);
        self.bus.bus.set_ui_mem_watches(&[]);
        self.bus.bus.ui_clear_beam_traps();
        self.bus.bus.ui_clear_copper_breaks();
        self.ui_stop = None;
    }

    /// Take the pending interactive break/watch hit, if any.
    pub fn take_ui_debug_stop(&mut self) -> Option<crate::debugger::DebugStop> {
        self.ui_promote_reg_hit();
        self.ui_stop.take()
    }

    /// Whether an interactive break/watch hit is waiting to be surfaced.
    /// Also promotes a register-watch hit recorded by the bus (which can
    /// land without an instruction retiring, e.g. a Copper write while
    /// the CPU sits in STOP).
    pub fn ui_debug_stop_pending(&mut self) -> bool {
        self.ui_promote_reg_hit();
        self.ui_stop.is_some()
    }

    /// Whether an interactive debugger condition could stop inside a
    /// speculative frame. Run-ahead remains off while one is armed so the
    /// debugger always stops on the committed timeline.
    pub fn ui_debug_stops_armed(&self) -> bool {
        self.ui_breaks.armed()
    }

    /// Whether the environment-driven headless debugger is observing the
    /// machine. Its counters, traces, and screenshots are host-side and must
    /// not see frames that will be rewound.
    pub fn headless_debugger_armed(&self) -> bool {
        self.dbg.is_some()
    }

    /// Arm (or, with `None`, disarm) the environment-style headless debugger
    /// programmatically. The debugger is a pure observer: its per-instruction
    /// hooks read machine state through side-effect-free peeks and never
    /// bill bus time, so arming it must not move the emulated timeline
    /// (tests/debugger_transparency.rs holds that guarantee). The one
    /// documented exception is a `[cpu] jit` machine, which drops back to
    /// the precise per-instruction loop while any hook is armed.
    pub fn arm_headless_debugger(&mut self, dbg: Option<crate::debugger::Debugger>) {
        self.dbg = dbg;
        self.note_jit_debug_fallback();
    }

    /// Arm the CPU-level diagnostic observers the environment normally arms
    /// (`COPPERLINE_DBG_MEMW`, `DBG_FC`, `DBG_SPREN`, `DBG_IRQ`,
    /// `DIAG_IPL`, `DIAG_PCHIST`, `DIAG_CRASH`), programmatically. Like the
    /// headless debugger they are pure observers -- peeks and host-side
    /// counters only -- and tests/debugger_transparency.rs holds them to
    /// that. The bus-level log-only knobs (`DBG_CIA`, `DBG_DSKLEN`,
    /// `DBG_BLIT`, `DBG_FRAMESTATE`) read the environment at their log
    /// sites and have no programmatic switch.
    pub fn arm_diagnostic_hooks(&mut self, hooks: DiagnosticHooks) {
        self.bus.dbg_memw_addr = hooks.memw_addr;
        self.dbg_fc_addr = hooks.frame_counter_addr;
        self.dbg_spren_on = hooks.spren_clear;
        self.bus.dbg_irq_window = hooks.irq_window;
        self.dbg_ipl_on = hooks.ipl_split;
        self.dbg_pc_on = hooks.pc_histogram;
        self.dbg_crash_on = hooks.crash_ring;
        self.note_jit_debug_fallback();
    }

    /// Log once when a JIT-enabled machine has been pushed onto the precise
    /// loop by a debug or diagnostic hook: the batch model bills time
    /// differently, so the run's timeline is no longer the undebugged JIT
    /// run's. Silent otherwise.
    fn note_jit_debug_fallback(&self) {
        if self.jit_enabled
            && !matches!(self.cpu.cpu_type, CpuType::M68000 | CpuType::M68010)
            && self.debug_hooks_armed()
        {
            log::warn!(
                "cpu jit: a debug or diagnostic hook is armed, so this run uses the \
                 precise per-instruction loop instead of the batch path; its \
                 timeline differs from an undebugged [cpu] jit run (drop jit to \
                 keep runs comparable)"
            );
        }
    }

    /// Why interactive or environment-driven debugger state cannot observe
    /// speculative frames. Unlike CPU and chipset state, these counters,
    /// files, and rolling views intentionally survive a machine restore.
    pub fn runahead_debug_block_reason(&self) -> Option<&'static str> {
        if self.ui_debug_stops_armed() {
            return Some("debugger stop conditions armed");
        }
        if let Some(reason) = self.bus.bus.runahead_debug_block_reason() {
            return Some(reason);
        }
        if self.headless_debugger_armed() {
            return Some("COPPERLINE_DBG_* debugger armed");
        }
        if self.ui_trace.is_some() {
            return Some("instruction trace active");
        }
        if self.ui_pc_history_enabled {
            return Some("debugger PC history active");
        }
        None
    }

    /// Promote a custom-register watch or beam-trap hit recorded by the
    /// bus (which sees every writer and every colour clock, including
    /// while the CPU sits in STOP) into the machine-level stop reason.
    fn ui_promote_reg_hit(&mut self) {
        if self.ui_stop.is_some() {
            return;
        }
        if let Some(hit) = self.bus.bus.take_ui_reg_hit() {
            self.ui_stop = Some(crate::debugger::DebugStop::ChipReg {
                off: hit.off,
                value: hit.value,
                source: hit.source,
                vpos: hit.vpos,
                hpos: hit.hpos,
            });
            return;
        }
        if let Some((vpos, hpos)) = self.bus.bus.take_ui_beam_hit() {
            self.ui_stop = Some(crate::debugger::DebugStop::Beam { vpos, hpos });
            return;
        }
        if let Some((pc, vpos, hpos)) = self.bus.bus.take_ui_copper_hit() {
            self.ui_stop = Some(crate::debugger::DebugStop::CopperBreak { pc, vpos, hpos });
        }
    }

    /// Post-instruction interactive break/watch check. Breakpoints match
    /// the NEXT instruction's address, i.e. the machine stops before the
    /// breakpointed instruction executes (and resuming does not re-trip
    /// it). Memory watches compare the live word against the value seen
    /// when the watch was set or last hit, so writes by any bus master
    /// (CPU, Copper, blitter, disk DMA) are caught; the reported writer PC
    /// is the instruction that just retired.
    fn ui_check_breaks_after_step(&mut self) {
        use crate::debugger::DebugStop;
        let pc = self.cpu.pc & self.cpu.address_mask;
        // Drain read latches even when a higher-priority stop wins, so a
        // breakpoint cannot leave a stale read to fire after resume.
        let read_hits = self.bus.bus.take_ui_mem_reads();
        // Exception catchpoints: the core records every exception entry
        // (trap, fault, or interrupt) as it loads the handler vector;
        // drain it here so a hit stops at the handler's first
        // instruction.
        if let Some(vector) = self.cpu.last_exception_vector.take() {
            let vector = vector.min(u32::from(u16::MAX)) as u16;
            if self.ui_breaks.catches.contains(&vector) {
                self.ui_stop = Some(DebugStop::Exception { vector, pc });
                return;
            }
        }
        if self.ui_breakpoint_stops(pc) {
            self.ui_stop = Some(DebugStop::Breakpoint { pc });
            return;
        }
        self.ui_check_task_catch();
        if self.ui_stop.is_some() {
            return;
        }
        self.ui_check_loadseg_catch();
        if self.ui_stop.is_some() {
            return;
        }
        self.ui_promote_reg_hit();
        if self.ui_stop.is_some() {
            return;
        }
        let writer_pc = self.cpu.ppc & self.cpu.address_mask;
        // A read-side DMA channel touching a watched word leaves the
        // value alone, so the compare loop below can never see it; the
        // channels latch the access on the bus and it is promoted here.
        for hit in read_hits {
            if let Some(index) = self.ui_breaks.watches.iter().position(|w| {
                w.addr == hit.addr
                    && w.access.reads()
                    && (w.pc.is_none()
                        || (hit.source == crate::debugger::WatchSource::Cpu
                            && w.pc == Some(writer_pc)))
            }) {
                let watch = &self.ui_breaks.watches[index];
                if watch.filter.is_none_or(|f| f.accepts(hit.source)) {
                    let value = self.bus.bus.peek_word_any(hit.addr);
                    // A read-modify-write instruction has completed its write
                    // by this boundary too. The read stop wins, but that write
                    // must not become a second stale stop on resume.
                    self.ui_breaks.watches[index].last = value;
                    let _ = self.bus.bus.ui_take_mem_writer(hit.addr);
                    self.ui_stop = Some(DebugStop::Watch {
                        addr: hit.addr,
                        old: value,
                        new: value,
                        writer_pc,
                        source: hit.source,
                        vpos: hit.vpos,
                        hpos: hit.hpos,
                        access: crate::debugger::WatchAccess::Read,
                    });
                    return;
                }
            }
        }
        for i in 0..self.ui_breaks.watches.len() {
            let addr = self.ui_breaks.watches[i].addr;
            let new = self.bus.bus.peek_word_any(addr);
            if new == self.ui_breaks.watches[i].last {
                continue;
            }
            let old = self.ui_breaks.watches[i].last;
            self.ui_breaks.watches[i].last = new;
            if !self.ui_breaks.watches[i].access.writes() {
                continue;
            }
            // Attribute the change: a DMA engine that flagged a write to
            // this word wins over the default CPU story.
            let writer = self.bus.bus.ui_take_mem_writer(addr);
            let (source, vpos, hpos) = match writer {
                Some(w) => (w.source, w.vpos, w.hpos),
                None => (
                    crate::debugger::WatchSource::Cpu,
                    self.bus.bus.agnus.vpos.min(u32::from(u16::MAX)) as u16,
                    self.bus.bus.agnus.hpos.min(u32::from(u16::MAX)) as u16,
                ),
            };
            if let Some(filter) = self.ui_breaks.watches[i].filter {
                if !filter.accepts(source) {
                    // Filtered out: the baseline moved on, no stop.
                    continue;
                }
            }
            if let Some(want) = self.ui_breaks.watches[i].pc {
                // The PC qualifier only speaks for CPU writes; a DMA
                // engine's write has no instruction behind it.
                if !matches!(source, crate::debugger::WatchSource::Cpu) || want != writer_pc {
                    continue;
                }
            }
            self.ui_stop = Some(DebugStop::Watch {
                addr,
                old,
                new,
                writer_pc,
                source,
                vpos,
                hpos,
                access: crate::debugger::WatchAccess::Write,
            });
            return;
        }
    }

    pub fn reset_after_bus_reset(&mut self) {
        self.reset_cpu();
    }

    pub fn refresh_irq_line(&mut self) -> u8 {
        // Apply any timed-device color clocks the last instruction's bus
        // accesses deferred before sampling the interrupt line. This runs at the
        // top of `service_pending_irq_cycles`, i.e. before every instruction, so
        // a device interrupt that came due during the previous instruction is
        // recognized at the correct boundary even within a multi-instruction
        // core slice.
        self.bus.flush_timed_devices_dcache_coherent();
        let mut level = self.pending_irq_level();
        // The boundary decision uses the IPL level latched at the previous
        // instruction's last bus access (CpuBus::sample_ipl), not the live
        // level: on real silicon a level that rose after that sample is taken
        // one instruction later. A STOPped CPU keeps sampling its pins, so it
        // wakes on the live level. min() so a level that dropped since the
        // sample (e.g. the instruction's own INTREQ clear) is never delivered
        // stale. COPPERLINE_IRQ_LATENCY_CCK=0 disables this together with the
        // IPL pipe (the raw immediate model, used by mechanism tests).
        // JIT batches run whole stretches of instructions without per-access
        // IPL latching, so the sampled level can be arbitrarily stale there;
        // a JIT machine takes the live level instead (the raw model).
        if self.bus.bus.irq_latency_setting != 0 && !self.cpu.is_stopped() && !self.jit_enabled {
            level = level.min(self.bus.sampled_irq_level);
        }
        // The boundary decision consumed the sample: release any poll-point
        // hold so the next instruction's accesses latch normally.
        self.bus.ipl_sample_held = false;
        self.cpu.set_irq(level);
        level
    }

    pub fn disable_overlay(&mut self) {
        self.bus.bus.mem.overlay = false;
        self.bus.bus.overlay_disable_pending = false;
        debug!("overlay disabled (chip RAM mapped at $0)");
    }

    /// Enable the fast batch/trace-JIT execution mode (`[cpu] jit`). Inert
    /// on the 68000/68010: their shared-bus float model needs the precise
    /// core (see `jit_fast_path_ok`), so neither the batch path nor the
    /// JIT's zero-wait external billing may engage there.
    pub fn set_jit_enabled(&mut self, on: bool) {
        if on && matches!(self.cpu.cpu_type, CpuType::M68000 | CpuType::M68010) {
            log::info!(
                "cpu jit: the 68000/68010 shared-bus float model needs the \
                 precise per-instruction core; [cpu] jit has no effect on \
                 this CPU (use a 68020 or later)"
            );
            return;
        }
        self.jit_enabled = on;
        self.bus.jit_enabled = on;
        self.note_jit_debug_fallback();
    }

    pub fn jit_enabled(&self) -> bool {
        self.jit_enabled
    }

    /// Whether `step_slice` may take the batch/JIT path right now: every
    /// per-instruction debug or diagnostic hook the precise loop services
    /// must be idle, and the FPU-absence shim must not be needed (the
    /// 68040 is the one model whose missing FPU only the host shim
    /// covers; everywhere else the core handles it natively).
    ///
    /// The 68000/68010 never take the batch path at all. On the small-box
    /// machines every CPU cycle drives the one bus shared with Agnus, and
    /// the floating-bus model (`note_cpu_data_bus`) is PREFETCH-ORDER
    /// dependent: Kickstart's diagnostic-ROM probe (`CMPI.W #$1111,(A1)`
    /// against unmapped $F00000) relies on the prefetch queue having
    /// fetched the following words so the float is NOT the immediate just
    /// read. The batch contract runs without prefetch, the float lands on
    /// the immediate, and the probe false-matches -- Kickstart jumps into
    /// empty space. `[cpu] jit` on these CPUs falls back to the precise
    /// loop (logged once at build).
    fn jit_fast_path_ok(&self) -> bool {
        if matches!(self.cpu.cpu_type, CpuType::M68000 | CpuType::M68010) {
            return false;
        }
        !(self.debug_hooks_armed() || (!self.fpu_enabled && self.cpu.cpu_type == CpuType::M68040))
    }

    /// Whether any per-instruction debug or diagnostic hook is armed: the
    /// headless debugger, the CPU-level `COPPERLINE_DBG_*`/`DIAG_*`
    /// observers, the interactive debugger's stops and traces, the
    /// waveform PC trigger and the self-modifying-code tracker. Each of
    /// these is serviced only by the precise loop, so together they are
    /// what pushes a JIT machine off the batch path (`jit_fast_path_ok`)
    /// and what `note_jit_debug_fallback` reports; the FPU-absence shim is
    /// a model property, not a hook, and is kept out of this predicate.
    fn debug_hooks_armed(&self) -> bool {
        self.dbg.is_some()
            || self.dbg_pc_on
            || self.dbg_crash_on
            || self.dbg_sched_on
            || self.dbg_fc_addr.is_some()
            || self.dbg_ipl_on
            || self.dbg_spren_on
            || self.bus.dbg_memw_addr.is_some()
            || self.ui_breaks.armed()
            || self.ui_pc_history_enabled
            || self.ui_trace.is_some()
            || {
                #[cfg(feature = "control")]
                {
                    self.profile_samples.is_some()
                }
                #[cfg(not(feature = "control"))]
                {
                    false
                }
            }
            || self.bus.bus.wave_pc_trigger
            || self.bus.bus.smc.is_some()
    }

    /// Select the diagnostic-capable precise loop once per slice instead of
    /// asking the same inactive questions after every retired instruction.
    /// The const-generic split below gives LLVM a branch-free normal build
    /// while preserving the exact existing path whenever any hook is armed.
    #[inline]
    fn precise_debug_hooks_active(&self) -> bool {
        self.dbg.is_some()
            || self.dbg_pc_on
            || self.dbg_crash_on
            || self.dbg_sched_on
            || self.dbg_fc_addr.is_some()
            || self.dbg_ipl_on
            || self.dbg_spren_on
            || self.bus.dbg_memw_addr.is_some()
            || self.ui_breaks.armed()
            || self.ui_stop.is_some()
            || self.ui_pc_history_enabled
            || self.ui_trace.is_some()
            || {
                #[cfg(feature = "control")]
                {
                    self.profile_samples.is_some()
                }
                #[cfg(not(feature = "control"))]
                {
                    false
                }
            }
            || self.bus.bus.wave_pc_trigger
            || self.bus.bus.smc.is_some()
            || diag_cpu_sync_on()
    }

    pub fn step_slice(&mut self, count: usize) -> Result<CpuStepSlice> {
        if self.jit_enabled && self.jit_fast_path_ok() {
            return self.step_slice_jit(count);
        }
        if self.precise_debug_hooks_active() {
            self.step_slice_precise::<true>(count)
        } else {
            self.step_slice_precise::<false>(count)
        }
    }

    fn step_slice_precise<const DEBUG_HOOKS: bool>(
        &mut self,
        count: usize,
    ) -> Result<CpuStepSlice> {
        self.bus.bus.begin_cpu_slice();
        self.bus.bus.slice_preempted = false;
        self.bus.bus.set_cpu_bus_arbitration_enabled(true);
        // This slice charges every instruction as it retires, so the shared
        // chip clock phase is current at each access.
        self.bus.bus.set_cpu_access_phase_sync(true);

        let mut instructions = 0usize;
        let mut cpu_cycles = 0u32;
        let mut cpu_cck = 0u32;
        let mut stopped = false;

        while instructions < count {
            let bus_before = self.bus.bus.slice_bus_advanced_cck();
            let cpu_cck_before = cpu_cck;
            #[cfg(feature = "control")]
            let irq_profile_start =
                self.profile_sample_start(self.cpu.pc, self.bus.bus.cpu_wait_cck_total());
            if let Some(irq) = self.service_pending_irq_cycles() {
                cpu_cycles = cpu_cycles.saturating_add(positive_cpu_cycles(irq.cycles));
                let irq_cck = self.charge_cpu_clocks_less_bus_overlap(irq.cycles);
                cpu_cck = cpu_cck.saturating_add(irq_cck);
                #[cfg(feature = "control")]
                if let Some(start) = irq_profile_start {
                    self.profile_finish_irq_sample(start, irq_cck, irq.level, irq.vector);
                }
                // An interrupt dispatch can land the PC on a breakpoint or
                // a caught vector; stop before the handler's first
                // instruction executes.
                if DEBUG_HOOKS && self.ui_breaks.armed() {
                    let pc = self.cpu.pc & self.cpu.address_mask;
                    if let Some(vector) = self.cpu.last_exception_vector.take() {
                        let vector = vector.min(u32::from(u16::MAX)) as u16;
                        if self.ui_breaks.catches.contains(&vector) {
                            self.ui_stop =
                                Some(crate::debugger::DebugStop::Exception { vector, pc });
                            break;
                        }
                    }
                    if self.ui_breakpoint_stops(pc) {
                        self.ui_stop = Some(crate::debugger::DebugStop::Breakpoint { pc });
                        break;
                    }
                }
            }
            #[cfg(feature = "control")]
            let profile_start =
                self.profile_sample_start(self.cpu.pc, self.bus.bus.cpu_wait_cck_total());
            if self.force_fpu_line_f_if_needed() {
                instructions = instructions.saturating_add(1);
                cpu_cycles = cpu_cycles.saturating_add(34);
                // The forced exception performs the same stack writes as an
                // IRQ or a core-raised exception. Consume any posted-write
                // overlap inside this exception's charge so it cannot leak
                // into the handler's first instruction.
                let instruction_cck = self.charge_cpu_clocks_less_bus_overlap(34);
                cpu_cck = cpu_cck.saturating_add(instruction_cck);
                #[cfg(feature = "control")]
                if let Some(start) = profile_start {
                    self.profile_finish_instruction_sample(start, instruction_cck);
                }
            } else {
                let dbg_pc_before = self.cpu.pc;
                let dbg_ipl_before = if DEBUG_HOOKS {
                    (self.cpu.get_sr() >> 8) & 7
                } else {
                    0
                };
                if DEBUG_HOOKS && self.dbg_pc_on {
                    self.dbg_record_pc();
                }
                if DEBUG_HOOKS && self.dbg_crash_on {
                    self.dbg_record_crash_path();
                }
                if DEBUG_HOOKS && self.dbg_sched_on {
                    self.debug_check_sched(dbg_pc_before);
                }
                let dbg_watch_snapshot = if DEBUG_HOOKS && self.dbg.is_some() {
                    self.debug_before_step()
                } else {
                    None
                };
                self.bus.diag_current_pc = dbg_pc_before;
                self.bus.bus.cpu_pc = dbg_pc_before;
                let cpu_was_stopped = self.cpu.is_stopped();
                // The no-op handler declines every trap, so the CPU takes the
                // real exception -- the plain step() would surface traps as
                // StepResults instead of raising them.
                match self
                    .cpu
                    .step_with_hle_handler(&mut self.bus, &mut m68k::NoOpHleHandler)
                {
                    StepResult::Ok { cycles } => {
                        if DEBUG_HOOKS && self.bus.bus.smc.is_some() {
                            // Mark the instruction just retired as code.
                            // Its own span is the honest answer: the
                            // prefetch queue also reads a word past the
                            // stream, which the CPU may never execute.
                            let after = self.cpu.pc & self.cpu.address_mask;
                            let span = after.wrapping_sub(dbg_pc_before);
                            let len = if span > 0 && span <= MAX_INSTRUCTION_BYTES {
                                span
                            } else {
                                2
                            };
                            if let Some(smc) = self.bus.bus.smc.as_mut() {
                                smc.mark_executed(dbg_pc_before, len);
                            }
                        }
                        if self.bus.icache.is_some() || self.bus.dcache.is_some() {
                            self.apply_cacr_updates();
                        }
                        instructions = instructions.saturating_add(1);
                        cpu_cycles = cpu_cycles.saturating_add(positive_cpu_cycles(cycles));
                        let instruction_cck = self.charge_cpu_clocks_less_bus_overlap(cycles);
                        cpu_cck = cpu_cck.saturating_add(instruction_cck);
                        #[cfg(feature = "control")]
                        if let Some(start) = profile_start {
                            self.profile_finish_instruction_sample(start, instruction_cck);
                        }
                        if DEBUG_HOOKS {
                            if let Some(snapshot) = dbg_watch_snapshot {
                                self.debug_after_step(snapshot);
                            }
                            self.debug_check_spren_clear();
                            self.debug_check_frame_counter();
                            self.debug_check_memw();
                            if self.ui_pc_history_enabled {
                                self.ui_pc_history[self.ui_pc_history_next] = dbg_pc_before;
                                self.ui_pc_history_next =
                                    (self.ui_pc_history_next + 1) % UI_PC_HISTORY_CAP;
                                self.ui_pc_history_len =
                                    (self.ui_pc_history_len + 1).min(UI_PC_HISTORY_CAP);
                            }
                            if self.ui_trace.is_some() {
                                self.ui_trace_record(dbg_pc_before);
                            }
                            if self.bus.bus.wave_pc_trigger {
                                self.bus
                                    .bus
                                    .wave_note_pc(dbg_pc_before & self.cpu.address_mask);
                            }
                            self.debug_check_ipl(dbg_ipl_before, positive_cpu_cycles(cycles));
                            if self.ui_breaks.armed() {
                                self.ui_check_breaks_after_step();
                            }
                            if self.dbg_pc_on && self.dbg_in_window {
                                *self.dbg_pc_cyc.entry(dbg_pc_before).or_insert(0) +=
                                    u64::from(positive_cpu_cycles(cycles));
                            }
                        }
                    }
                    StepResult::Stopped => {
                        if !cpu_was_stopped {
                            self.bus.bus.note_cpu_stop(false);
                        }
                        stopped = true;
                        break;
                    }
                    other => {
                        return Err(anyhow!(
                            "unexpected m68k step result {:?} at PC={:#010X}",
                            other,
                            self.cpu.pc
                        ));
                    }
                }
            }

            // A 020+ shares one clock timeline with the chip bus: the charge
            // above already advanced the chipset through the execution time
            // that did not overlap an access, and each access credited its own
            // clocks back, so there is nothing left to reconcile. The 68000
            // keeps its two timelines and the reconciliation below, which can
            // only ever add time.
            if self.sync_cck_on && !self.bus.bus.cpu_short_bus_cycle() {
                // Advance the chipset through this instruction's CPU-internal
                // cycles (cpu_cck minus the cycles already spent on the bus) so
                // the beam and DMA track the CPU's full instruction time.
                let cpu_cck_iter = cpu_cck.saturating_sub(cpu_cck_before);
                let bus_iter = self
                    .bus
                    .bus
                    .slice_bus_advanced_cck()
                    .saturating_sub(bus_before);
                if DEBUG_HOOKS && diag_cpu_sync_on() && diag_cpu_sync_pc_matches(self.cpu.pc) {
                    eprintln!(
                        "CPUSYNC boundary pc={:08x} cpu_cck={} bus_cck={} rem_cck={} v={:03x} h={:02x}",
                        self.cpu.pc,
                        cpu_cck_iter,
                        bus_iter,
                        cpu_cck_iter.saturating_sub(bus_iter),
                        self.bus.bus.agnus.vpos,
                        self.bus.bus.agnus.hpos,
                    );
                }
                self.bus
                    .bus
                    .advance_cpu_internal_cycles(cpu_cck_iter.saturating_sub(bus_iter));
            }

            // Stop the slice at an interactive breakpoint/watch hit; the
            // emulator ends the frame early and the window pauses.
            if DEBUG_HOOKS && self.ui_stop.is_some() {
                break;
            }

            if self.bus.bus.slice_preempted {
                break;
            }
        }

        // Apply the final instruction's deferred timed-device color clocks
        // (per-instruction flushes happen at the top of the next
        // `refresh_irq_line`, so the last one would otherwise stay pending).
        // This keeps `pending_device_cck` zero at every slice boundary, where
        // save states are taken -- the accumulator is not serialized.
        self.bus.flush_timed_devices_dcache_coherent();
        self.bus.bus.set_cpu_bus_arbitration_enabled(false);
        let (bus_advanced_cck, _bus_tick) = self.bus.bus.take_slice_bus_advance();
        if stopped {
            trace!(
                "m68k CPU stopped at PC={:#010X} SR={:#06X}",
                self.pc(),
                self.sr()
            );
        }
        Ok(CpuStepSlice {
            instructions,
            cpu_cycles,
            cpu_cck,
            bus_advanced_cck,
            stopped,
        })
    }

    /// Fast execution slice for `[cpu] jit`: run the core's
    /// instruction-budgeted batch path (natively compiled hot traces with
    /// the `cpu-jit` build feature, the interpreted trace loop without)
    /// instead of cycle-stepping one instruction at a time.
    ///
    /// The timing contract is deliberately approximate -- the machine
    /// behaves like one with an accelerator card fitted:
    /// - every retired instruction is billed a flat
    ///   `JIT_CPU_CLOCKS_PER_INSTRUCTION` CPU clocks, converted to chipset
    ///   time at the configured clock ratio exactly like the precise path;
    /// - chip-bus accesses still arbitrate into DMA slots through `CpuBus`
    ///   and advance the beam as they land, so chipset side effects stay
    ///   ordered against display DMA, just not cycle-exactly;
    /// - accesses inside the fastmem window (the largest fast-RAM bank, see
    ///   `CpuBus::fast_mem`) cost nothing at all;
    /// - interrupts are recognized at batch boundaries (and once inside
    ///   each batch entry), so their latency is bounded by
    ///   `JIT_BATCH_INSTRUCTIONS` rather than modelled.
    fn step_slice_jit(&mut self, count: usize) -> Result<CpuStepSlice> {
        self.bus.bus.begin_cpu_slice();
        self.bus.bus.slice_preempted = false;
        self.bus.bus.set_cpu_bus_arbitration_enabled(true);
        // A batch is charged as one lump, so the shared chip clock phase would
        // be stale at every access after the first. This slice keeps the older
        // per-slice accounting, matching the batch model's stated coarseness.
        self.bus.bus.set_cpu_access_phase_sync(false);

        let mut instructions = 0usize;
        let mut cpu_cycles = 0u32;
        let mut cpu_cck = 0u32;
        let mut stopped = false;

        while instructions < count {
            let bus_before = self.bus.bus.slice_bus_advanced_cck();
            let cpu_cck_before = cpu_cck;
            if let Some(irq) = self.service_pending_irq_cycles() {
                cpu_cycles = cpu_cycles.saturating_add(positive_cpu_cycles(irq.cycles));
                cpu_cck = cpu_cck.saturating_add(self.charge_cpu_clocks(irq.cycles));
            }
            // A level the boundary could not deliver (the CPU is still
            // inside a handler at that priority) must not ride into the
            // batch: the batch core would service it the moment RTE drops
            // the mask WITHOUT recomputing INTREQ, spuriously re-entering
            // a handler whose request was already cleared. Interrupts are
            // delivered exclusively at slice boundaries, from live state.
            self.cpu.set_irq(0);
            let budget = (count - instructions).min(JIT_BATCH_INSTRUCTIONS) as u32;
            self.bus.diag_current_pc = self.cpu.pc;
            self.bus.bus.cpu_pc = self.cpu.pc;
            let cpu_was_stopped = self.cpu.is_stopped();
            let result = self.cpu.run_batch(&mut self.bus, budget, &[]);
            if self.bus.icache.is_some() || self.bus.dcache.is_some() {
                // A CACR write inside the batch (MOVEC) must reach the
                // cache models -- enables, disables, and flushes -- just
                // as the precise loop syncs it per instruction.
                self.apply_cacr_updates();
            }
            instructions = instructions.saturating_add(result.instructions as usize);
            let mut batch_clocks = result
                .instructions
                .saturating_mul(JIT_CPU_CLOCKS_PER_INSTRUCTION);
            let mut exit_stopped = false;
            match result.exit {
                BatchExit::BudgetExhausted | BatchExit::WatchedPc { .. } => {}
                BatchExit::Stopped => exit_stopped = true,
                // The batch path surfaces traps instead of taking them; the
                // precise loop's no-op HLE policy is to let every one become
                // its real hardware exception. Mirror that here, billing the
                // exception entry and counting the trapping instruction.
                BatchExit::AlineTrap { .. } => {
                    let cycles = self.cpu.take_aline_exception(&mut self.bus);
                    batch_clocks = batch_clocks
                        .saturating_add(positive_cpu_cycles(cycles))
                        .saturating_add(JIT_CPU_CLOCKS_PER_INSTRUCTION);
                    instructions = instructions.saturating_add(1);
                }
                BatchExit::FlineTrap { .. } => {
                    let cycles = self.cpu.take_fline_exception(&mut self.bus);
                    batch_clocks = batch_clocks
                        .saturating_add(positive_cpu_cycles(cycles))
                        .saturating_add(JIT_CPU_CLOCKS_PER_INSTRUCTION);
                    instructions = instructions.saturating_add(1);
                }
                BatchExit::TrapInstruction { trap_num } => {
                    let cycles = self.cpu.take_trap_exception(&mut self.bus, trap_num);
                    batch_clocks = batch_clocks
                        .saturating_add(positive_cpu_cycles(cycles))
                        .saturating_add(JIT_CPU_CLOCKS_PER_INSTRUCTION);
                    instructions = instructions.saturating_add(1);
                }
                BatchExit::Breakpoint { .. } => {
                    let cycles = self.cpu.take_bkpt_exception(&mut self.bus);
                    batch_clocks = batch_clocks
                        .saturating_add(positive_cpu_cycles(cycles))
                        .saturating_add(JIT_CPU_CLOCKS_PER_INSTRUCTION);
                    instructions = instructions.saturating_add(1);
                }
                BatchExit::IllegalInstruction { .. } => {
                    let cycles = self.cpu.take_illegal_exception(&mut self.bus);
                    batch_clocks = batch_clocks
                        .saturating_add(positive_cpu_cycles(cycles))
                        .saturating_add(JIT_CPU_CLOCKS_PER_INSTRUCTION);
                    instructions = instructions.saturating_add(1);
                }
            }
            cpu_cycles = cpu_cycles.saturating_add(batch_clocks);
            cpu_cck = cpu_cck
                .saturating_add(self.charge_cpu_clocks_less_bus_overlap(batch_clocks as i32));

            if self.sync_cck_on {
                // Advance the chipset through the batch's billed CPU time
                // beyond what its bus accesses already advanced, exactly as
                // the precise loop does per instruction.
                let cpu_cck_iter = cpu_cck.saturating_sub(cpu_cck_before);
                let bus_iter = self
                    .bus
                    .bus
                    .slice_bus_advanced_cck()
                    .saturating_sub(bus_before);
                self.bus
                    .bus
                    .advance_cpu_internal_cycles(cpu_cck_iter.saturating_sub(bus_iter));
            }

            if exit_stopped {
                if !cpu_was_stopped {
                    self.bus.bus.note_cpu_stop(false);
                }
                stopped = true;
                break;
            }
            if self.bus.bus.slice_preempted {
                break;
            }
        }

        self.bus.flush_timed_devices_dcache_coherent();
        self.bus.bus.set_cpu_bus_arbitration_enabled(false);
        let (bus_advanced_cck, _bus_tick) = self.bus.bus.take_slice_bus_advance();
        if stopped {
            trace!(
                "m68k CPU stopped at PC={:#010X} SR={:#06X}",
                self.pc(),
                self.sr()
            );
        }
        Ok(CpuStepSlice {
            instructions,
            cpu_cycles,
            cpu_cck,
            bus_advanced_cck,
            stopped,
        })
    }

    /// Convert an instruction's CPU clocks to colour clocks of chipset time at
    /// the configured clock ratio. Sub-cck remainders carry across calls so an
    /// accelerated CPU (many clocks per cck) accumulates fractional cck
    /// instead of being rounded up to a whole cck per instruction.
    fn charge_cpu_clocks(&mut self, cycles: i32) -> u32 {
        self.charge_cpu_clock_value(positive_cpu_cycles(cycles))
    }

    /// Charge instruction clocks less the transfer clocks a posted chip write
    /// overlapped with execution on the decoupled 020+ bus unit -- the beam
    /// must not advance for time the bus unit hid under the instruction.
    fn charge_cpu_clocks_less_bus_overlap(&mut self, cycles: i32) -> u32 {
        let overlap = self.bus.bus.take_cpu_bus_overlap_clocks();
        self.charge_cpu_clock_value(positive_cpu_cycles(cycles).saturating_sub(overlap))
    }

    fn charge_cpu_clock_value(&mut self, clocks: u32) -> u32 {
        if self.bus.bus.cpu_short_bus_cycle() && self.bus.bus.cpu_access_phase_sync() {
            // The 020+ shares one clock timeline with the chip bus, so the
            // sub-colour-clock phase lives there and an access can see where
            // in the colour clock the CPU is; see `Bus::cpu_chip_clock_phase`.
            return self.bus.bus.charge_cpu_clocks_to_cck(clocks);
        }
        let total = clocks + self.cpu_clock_carry;
        let cck = total / self.cpu_clocks_per_cck;
        self.cpu_clock_carry = total % self.cpu_clocks_per_cck;
        cck
    }

    /// Install the 68020/030/040 cache models. These default on for CPUs that
    /// have them (see `CpuModel::has_instruction_cache`/`has_data_cache`),
    /// matching real silicon where AmigaOS enables the cache via CACR; `[cpu]
    /// icache = false`/`dcache = false` opt out. With both false, CACR writes
    /// are tracked but have no effect. The cache is sized for the CPU: 256
    /// bytes (64 longwords) on the 020/030, 4 KB (1024) on the 040.
    pub fn set_cache_emulation(&mut self, icache: bool, dcache: bool) {
        let lines = cache_lines_for_cpu_type(self.cpu.cpu_type);
        self.bus.icache = icache.then(|| Box::new(crate::cache::CpuCache::new(lines)));
        self.bus.dcache = dcache.then(|| Box::new(crate::cache::CpuCache::new(lines)));
        self.last_cacr = 0;
        self.apply_cacr_updates();
        if icache || dcache {
            log::info!(
                "cpu cache emulation: icache={} dcache={} (CACR-controlled)",
                icache,
                dcache
            );
        }
    }

    /// Push CACR state into the cache models and run any pending clear
    /// strobes. Called after every instruction; a single compare when
    /// nothing changed.
    fn apply_cacr_updates(&mut self) {
        use m68k::{
            CACR_040_DE, CACR_040_IE, CACR_CD, CACR_CED, CACR_CEI, CACR_CI, CACR_ED, CACR_EI,
            CACR_FD, CACR_FI,
        };
        let ops = std::mem::take(&mut self.cpu.cacr_pending_ops);
        let cacr = self.cpu.cacr;
        if ops == 0 && cacr == self.last_cacr {
            return;
        }
        self.last_cacr = cacr;
        let caar = self.cpu.caar;
        // The 68040/68060 CACR uses different enable bits (IE/DE - the 060's
        // EIC/EDC sit at the same positions) and has no freeze or CACR clear
        // strobes - invalidation arrives through CINV/CPUSH, which decode.rs
        // surfaces as the CACR_CI/CACR_CD pending ops below. The 060's
        // branch-cache strobes (CABC/CUBC) are consumed inside the core.
        if matches!(
            self.cpu.cpu_type,
            CpuType::M68EC040 | CpuType::M68LC040 | CpuType::M68040 | CpuType::M68060
        ) {
            if let Some(icache) = self.bus.icache.as_deref_mut() {
                icache.enabled = cacr & CACR_040_IE != 0;
                icache.frozen = false;
                if ops & CACR_CI != 0 {
                    icache.clear_all();
                }
            }
            if let Some(dcache) = self.bus.dcache.as_deref_mut() {
                dcache.enabled = cacr & CACR_040_DE != 0;
                dcache.frozen = false;
                if ops & CACR_CD != 0 {
                    dcache.clear_all();
                }
            }
            return;
        }
        if let Some(icache) = self.bus.icache.as_deref_mut() {
            icache.enabled = cacr & CACR_EI != 0;
            icache.frozen = cacr & CACR_FI != 0;
            if ops & CACR_CI != 0 {
                icache.clear_all();
            }
            if ops & CACR_CEI != 0 {
                icache.clear_entry_by_index(caar);
            }
        }
        if let Some(dcache) = self.bus.dcache.as_deref_mut() {
            dcache.enabled = cacr & CACR_ED != 0;
            dcache.frozen = cacr & CACR_FD != 0;
            if ops & CACR_CD != 0 {
                dcache.clear_all();
            }
            if ops & CACR_CED != 0 {
                dcache.clear_entry_by_index(caar);
            }
        }
    }

    fn reset_cpu(&mut self) {
        self.cpu.reset(&mut self.bus);
        self.cpu.set_sr(0x2700);
        self.apply_cacr_updates();
        debug!(
            "reset vector: SP={:#010X} PC={:#010X} SR={:#06X}",
            self.cpu.sp(),
            self.cpu.pc,
            self.cpu.get_sr()
        );
    }

    fn service_pending_irq_cycles(&mut self) -> Option<IrqDispatch> {
        let level = self.refresh_irq_line();
        if !self.cpu.check_interrupts() {
            return None;
        }
        let was_stopped = self.cpu.is_stopped();
        // Recognition belongs to this instruction boundary. Exception stack
        // writes and vector fetches advance the bus, so emitting afterwards
        // incorrectly stamps the event at the end of dispatch.
        self.bus.bus.note_cpu_irq_recognized(level);
        if was_stopped {
            self.bus.bus.note_cpu_stop(true);
        }
        let cycles = self.cpu.execute(&mut self.bus, 0);
        #[cfg(feature = "control")]
        let vector = self
            .cpu
            .last_exception_vector
            .unwrap_or(u32::from(level) + 24)
            .min(u32::from(u16::MAX)) as u16;
        Some(IrqDispatch {
            cycles,
            #[cfg(feature = "control")]
            level,
            #[cfg(feature = "control")]
            vector,
        })
    }

    fn pending_irq_level(&self) -> u8 {
        let bus = &self.bus.bus;
        // A freezer cartridge's button drives IPL to 7: non-maskable, so
        // Paula's INTENA and the SR mask have no say. The vector was just
        // written, so the bring-up guard below does not apply.
        if bus.cartridge_nmi_pending() {
            return 7;
        }
        if bus.paula.intena & INT_MASTER == 0 {
            return 0;
        }
        let pending = bus.paula.intena & bus.cpu_visible_intreq();
        let level = pending_ipl(pending);
        if level == 0 {
            return 0;
        }

        // Bring-up guard: hold off an autovector whose handler slot still
        // reads zero. The peek is untranslated, so it only means anything
        // while the MMU is off (with translation enabled, VBR holds a
        // virtual address and the physical bytes under it are unrelated --
        // Linux/m68k maps its vector table high while low chip RAM is
        // cleared, and the guard would silence every interrupt for good).
        if !self.cpu.tc_enable() {
            let vector_addr = self
                .cpu
                .vbr
                .wrapping_add(0x60)
                .wrapping_add(u32::from(level) * 4);
            if self.bus.peek_long(vector_addr) == 0 {
                return 0;
            }
        }
        level
    }

    fn force_fpu_line_f_if_needed(&mut self) -> bool {
        if self.fpu_enabled {
            return false;
        }
        // The 060 models FPU absence in the core via PCR.DFP (set at
        // construction), with the proper format $4 disabled frame.
        if self.cpu.is_060() {
            return false;
        }
        let pc = self.cpu.pc;
        let opword = self.bus.peek_word(pc);
        if !is_fpu_instruction_family(opword) {
            return false;
        }

        let _ = self.bus.read_immediate_word(pc);
        self.cpu.ppc = pc;
        self.cpu.sr_save = self.cpu.get_sr();
        self.cpu.ir = opword as u32;
        let _cycles = self.cpu.take_fline_exception(&mut self.bus);
        true
    }
}

/// Which plain-memory region a CPU-visible address falls in, and the byte
/// offset into that region's backing store. Shared by every access path
/// (peek/debug-write for the debugger, and the real uncached read/write)
/// so the plain-memory part of the address map -- overlay ROM, chip RAM,
/// Zorro RAM, slow RAM, motherboard RAM, accelerator RAM, ROM, WCS,
/// extended ROM -- is only
/// decoded in one place. I/O regions (CIA, RTC, Gayle, custom chips, Zorro devices,
/// autoconfig) are not plain memory and stay in `read_sized_uncached` /
/// `write_sized`, which only reach them once this classifier returns
/// `None`.
enum PlainMemRegion {
    OverlayRom(usize),
    ChipRam(usize),
    ZorroRam(usize, usize),
    SlowRam(usize),
    MbRam(usize),
    AccelRam(usize),
    Rom(usize),
    Wcs(usize),
    ExtendedRom(usize),
    /// The freezer cartridge's RAM bank (`crate::cartridge`).
    Cartridge(usize),
}

impl CpuBus {
    fn mask(&self, address: u32) -> u32 {
        address & self.address_mask
    }

    /// Flush deferred timed-device colour clocks, then drop the data-cache
    /// model if any device's tick DMA'd into guest memory (copperhf
    /// draining a completed read into the caller's fast-RAM buffer, a
    /// plugin board writing a buffer). This is the asynchronous
    /// counterpart of the `clear_all` the synchronous device-access path
    /// below already performs on `DeviceHost::touched_memory`: without it,
    /// the DMA'd data reads back stale through the modelled 030/040/060
    /// data cache (PFS3 on a 68040 read corrupted file data through
    /// copperhf until this existed). Checked unconditionally -- the flag
    /// can be pending from an idle-CPU device advance even when no colour
    /// clocks are.
    fn flush_timed_devices_dcache_coherent(&mut self) {
        self.bus.flush_timed_devices();
        if self.bus.take_devices_wrote_memory() {
            if let Some(dcache) = self.dcache.as_deref_mut() {
                dcache.clear_all();
            }
        }
    }

    fn classify_plain_memory(&self, addr: u32, size: usize) -> Option<PlainMemRegion> {
        if let Some(off) = self.overlay_rom_offset(addr, size) {
            return Some(PlainMemRegion::OverlayRom(off));
        }
        if let Some(off) = self.chip_window_offset(addr, size) {
            return Some(PlainMemRegion::ChipRam(off));
        }
        if let Some((board, off)) = self.bus.mem.zorro.region_at(addr, size) {
            return Some(PlainMemRegion::ZorroRam(board, off));
        }
        if let Some(off) = region_offset(self.bus.mem.slow_ram.len(), SLOW_RAM_BASE, addr, size) {
            return Some(PlainMemRegion::SlowRam(off));
        }
        if let Some(off) = region_offset(
            self.bus.mem.mb_ram.len(),
            self.bus.mem.mb_ram_base(),
            addr,
            size,
        ) {
            return Some(PlainMemRegion::MbRam(off));
        }
        if let Some(off) = region_offset(self.bus.mem.accel_ram.len(), ACCEL_RAM_BASE, addr, size) {
            return Some(PlainMemRegion::AccelRam(off));
        }
        if let Some(off) = region_offset(self.bus.mem.rom.len(), ROM_BASE, addr, size) {
            return Some(PlainMemRegion::Rom(off));
        }
        if let Some(off) = region_offset(self.bus.mem.wcs.len(), WCS_BASE, addr, size) {
            return Some(PlainMemRegion::Wcs(off));
        }
        if let Some(off) = region_offset(
            self.bus.mem.extended_rom.len(),
            self.bus.mem.extended_rom_base,
            addr,
            size,
        ) {
            return Some(PlainMemRegion::ExtendedRom(off));
        }
        if let Some(off) = self
            .bus
            .cartridge
            .as_ref()
            .and_then(|c| c.offset(addr, size))
        {
            return Some(PlainMemRegion::Cartridge(off));
        }
        None
    }

    /// CPU-side chip RAM decode. The motherboard address decode (Gary and
    /// equivalents) routes the whole $000000-$1FFFFF window to Agnus, and
    /// Agnus only decodes as many address bits as its DRAM reach
    /// (dma_addr_capability_mask): an OCS 8370/8371 ignores A19/A20, so its
    /// 512 KiB image repeats at $080000/$100000/$180000; the 1 MiB 8372A
    /// ignores A20, repeating its 1 MiB image at $100000; the 2 MiB
    /// 8375/Alice decode the full window with no repeat. Within one image,
    /// addresses past the fitted RAM select no DRAM bank and stay open bus
    /// (e.g. $080000-$0FFFFF on an 8372A with 512 KiB fitted). Kickstart's
    /// chip sizing copes: it detects the wrap by checking whether a probe
    /// write aliased the bottom of RAM.
    fn chip_window_offset(&self, addr: u32, size: usize) -> Option<usize> {
        let len = self.bus.mem.chip_ram.len();
        if len == 0 || u64::from(addr) >= CHIP_WINDOW_SIZE {
            return None;
        }
        let off = (addr & self.bus.agnus.revision().dma_addr_capability_mask()) as usize;
        (off.checked_add(size)? <= len).then_some(off)
    }

    /// Canonical custom-register page offset for a CPU access that the
    /// motherboard address decode routes to the custom chips: the canonical
    /// $DFF000 page on every machine, plus the mirrors Gary's coarse decode
    /// creates on the small-box machines (A1000/A500/A2000/CDTV). There the
    /// whole chip-register space answers custom-register accesses wherever
    /// no RAM or motherboard device claims the address: the empty
    /// trapdoor/"ranger" space above slow RAM in $C00000-$D7FFFF, the
    /// reserved $D80000-$DBFFFF pages, and $DE0000-$DFEFFF. Kickstart's
    /// slow-RAM sizing depends on the mirror to find the end of trapdoor
    /// RAM: exec writes INTENA at $xxF09A and reads INTENAR at $xxF01C at
    /// each 256K step, extending RAM until the mirror answers with the bits
    /// it just cleared. KS 1.2 trusts that test alone, so a floating bus
    /// there makes it size RAM to $DC0000 and crash into unmapped space
    /// (yellow screen); KS 1.3 added a write/read-back check that also
    /// stops on a floating bus. The big-box bus controllers decode this
    /// space properly instead of mirroring -- Gayle (A600/A1200) claims it
    /// and Fat Gary/Ramsey (A3000/A4000) time unanswered cycles out -- so
    /// machines with any of them get no mirror.
    fn custom_reg_page_offset(&self, addr: u32) -> Option<u32> {
        if range_contains(CUSTOM_BASE, CUSTOM_SIZE, addr) {
            return Some(addr & 0xFFF);
        }
        if self.bus.gayle.is_some() || self.bus.ramsey.is_some() || self.bus.gary.is_some() {
            return None;
        }
        let ranger = (0x00C0_0000..0x00D8_0000).contains(&addr)
            && region_offset(self.bus.mem.slow_ram.len(), SLOW_RAM_BASE, addr, 1).is_none();
        let reserved = (0x00D8_0000..0x00DC_0000).contains(&addr);
        // $DC0000 stays carved out for the RTC select (floating when no
        // clock answers) and $DD0000 is undecoded, as on real boards.
        let below_canonical = (0x00DE_0000..CUSTOM_BASE).contains(&addr);
        (ranger || reserved || below_canonical).then_some(addr & 0xFFF)
    }

    /// The `$DC0000` clock page, a byte lane at a time.
    ///
    /// Both clock parts (MSM6242 and RP5C01) are four-bit chips wired to the
    /// low byte lane alone, so they answer on odd addresses and the even lane
    /// floats -- with or without a chip in the socket, since nothing else
    /// drives it either.
    ///
    /// The register select is A2-A5, so A1 does not reach the decode and each
    /// register answers at *both* of its odd bytes: register 0 at `+1` and
    /// `+3`, register 1 at `+5` and `+7`, and so on. AmigaOS addresses them at
    /// `base + N * 4 + 3` by convention, not because the part is deaf at `+1`.
    fn rtc_page_read(&mut self, addr: u32, size: usize) -> u32 {
        let mut value = 0u32;
        for i in 0..size {
            let byte = self.rtc_page_byte(addr.wrapping_add(i as u32));
            value = (value << 8) | u32::from(byte);
        }
        value
    }

    fn rtc_page_byte(&mut self, addr: u32) -> u8 {
        if addr & 1 == 0 {
            // The clock never drives this lane; it keeps the bus's charge.
            return (self.bus.data_bus >> 8) as u8;
        }
        if !self.bus.rtc_present() {
            return crate::rtc::EMPTY_SOCKET_LANE;
        }
        let secs = self.bus.emulated_seconds();
        self.bus.rtc.read(u64::from(addr - RTC_BASE), 1, secs) as u8
    }

    /// Writes take the same lanes as reads: a byte aimed at the even lane
    /// reaches nothing, and a wider access lands only its odd bytes.
    fn rtc_page_write(&mut self, addr: u32, size: usize, value: u32) {
        for i in 0..size {
            let addr = addr.wrapping_add(i as u32);
            if addr & 1 == 0 {
                continue;
            }
            let byte = u64::from(value >> (8 * (size - 1 - i)) & 0xFF);
            let secs = self.bus.emulated_seconds();
            self.bus
                .rtc
                .write(u64::from(addr - RTC_BASE), 1, byte, secs);
        }
    }

    fn peek_word(&self, address: u32) -> u16 {
        let addr = self.mask(address);
        ((self.peek_byte(addr) as u16) << 8) | self.peek_byte(addr.wrapping_add(1)) as u16
    }

    /// If COPPERLINE_DBG_MEMW is armed and this write [addr, addr+size) covers the
    /// watched word, record the post-write word value so the run loop can log
    /// the writer PC. Reads back via the same address decode as the CPU.
    fn dbg_note_memw(&mut self, addr: u32, size: usize) {
        let Some(watch) = self.dbg_memw_addr else {
            return;
        };
        let lo = self.mask(addr);
        let hi = lo.wrapping_add(size as u32);
        if watch >= lo && watch < hi {
            self.dbg_memw_hit = Some(self.peek_word(watch));
        }
    }

    fn peek_long(&self, address: u32) -> u32 {
        let addr = self.mask(address);
        ((self.peek_word(addr) as u32) << 16) | self.peek_word(addr.wrapping_add(2)) as u32
    }

    fn peek_byte(&self, address: u32) -> u8 {
        let addr = self.mask(address);
        match self.classify_plain_memory(addr, 1) {
            Some(PlainMemRegion::OverlayRom(off) | PlainMemRegion::Rom(off)) => {
                self.bus.mem.rom[off]
            }
            Some(PlainMemRegion::ChipRam(off)) => self.bus.mem.chip_ram[off],
            Some(PlainMemRegion::ZorroRam(board, off)) => self.bus.mem.zorro.board_ram(board)[off],
            Some(PlainMemRegion::SlowRam(off)) => self.bus.mem.slow_ram[off],
            Some(PlainMemRegion::MbRam(off)) => self.bus.mem.mb_ram[off],
            Some(PlainMemRegion::AccelRam(off)) => self.bus.mem.accel_ram[off],
            Some(PlainMemRegion::Wcs(off)) => self.bus.mem.wcs[off],
            Some(PlainMemRegion::ExtendedRom(off)) => self.bus.mem.extended_rom[off],
            Some(PlainMemRegion::Cartridge(off)) => {
                self.bus.cartridge.as_ref().map_or(0xFF, |c| c.bank()[off])
            }
            None => {
                if crate::uaelib::UaeLib::decodes(addr) {
                    if let Some(b) = self
                        .bus
                        .uaelib
                        .as_ref()
                        .and_then(|u| u.peek_byte(addr - crate::uaelib::UAELIB_BASE))
                    {
                        return b;
                    }
                }
                0xFF
            }
        }
    }

    fn debug_write_byte(&mut self, address: u32, value: u8) -> bool {
        let addr = self.mask(address);
        match self.classify_plain_memory(addr, 1) {
            Some(PlainMemRegion::ChipRam(off)) => {
                self.bus.mem.chip_ram[off] = value;
                self.invalidate_debug_write(addr, 1);
                true
            }
            Some(PlainMemRegion::ZorroRam(board, off)) => {
                self.bus.mem.zorro.board_ram_mut(board)[off] = value;
                self.invalidate_debug_write(addr, 1);
                true
            }
            Some(PlainMemRegion::SlowRam(off)) => {
                self.bus.mem.slow_ram[off] = value;
                self.invalidate_debug_write(addr, 1);
                true
            }
            Some(PlainMemRegion::MbRam(off)) => {
                self.bus.mem.mb_ram[off] = value;
                self.invalidate_debug_write(addr, 1);
                true
            }
            Some(PlainMemRegion::AccelRam(off)) => {
                self.bus.mem.accel_ram[off] = value;
                self.invalidate_debug_write(addr, 1);
                true
            }
            Some(PlainMemRegion::Cartridge(off)) => {
                if let Some(cartridge) = self.bus.cartridge.as_mut() {
                    cartridge.bank_mut()[off] = value;
                }
                self.invalidate_debug_write(addr, 1);
                true
            }
            // Overlay ROM/ROM/WCS/extended ROM are not debugger-writable.
            _ => false,
        }
    }

    fn invalidate_debug_write(&mut self, addr: u32, size: usize) {
        if let Some(cache) = self.icache.as_deref_mut() {
            cache.invalidate_write(addr, size);
        }
        if let Some(cache) = self.dcache.as_deref_mut() {
            cache.invalidate_write(addr, size);
        }
    }

    /// Word-granular bus cycles for a CPU access of `size` bytes (the 68000
    /// performs one bus cycle per 16-bit word).
    fn access_words(size: usize) -> u32 {
        size.max(1).div_ceil(2) as u32
    }

    /// Bill an external-bus (fast RAM/ROM/WCS) access. In JIT mode the CPU
    /// models an ideal accelerator with zero-wait external memory -- the
    /// batch path bills one flat clock per instruction instead -- so these
    /// accesses cost nothing; chip/slow RAM (shared silicon, arbitrated)
    /// and the E-clock devices keep their real costs either way.
    fn bill_external_access(&mut self, size: usize) {
        if self.jit_enabled {
            return;
        }
        self.bus.cpu_external_access(Self::access_words(size));
    }

    /// Latch the last word the CPU drove onto (or sampled from) the shared
    /// data bus: undriven (unmapped) reads float to this value. A byte
    /// cycle drives one lane while the other keeps its previous charge.
    /// DMA fetches latch through their own paths (dma_slots /
    /// frame_capture); cache hits perform no bus cycle and do not latch.
    fn note_cpu_data_bus(&mut self, addr: u32, size: usize, value: u32) {
        // Only the 68000 shares the 16-bit chip data bus directly with
        // Agnus, which is the float-to-last-value behavior modeled here
        // (and what the vAmigaTS uninit tests pin down). The 020+ run on
        // their own wider local bus with different undriven behavior;
        // KS3.1-era memory probing misdetects if their reads echo here.
        if self.bus.cpu_short_bus_cycle() {
            return;
        }
        self.bus.data_bus = if size == 1 {
            let b = (value & 0xFF) as u16;
            if addr & 1 == 0 {
                (self.bus.data_bus & 0x00FF) | (b << 8)
            } else {
                (self.bus.data_bus & 0xFF00) | b
            }
        } else {
            // The last word of the transfer stays on the bus.
            (value & 0xFFFF) as u16
        };
    }

    /// Consume a debugger-injected bus fault covering this access, if
    /// one is armed. The access does not reach memory when it fires, so
    /// the guest sees exactly what an undecoded address gives it.
    fn check_injected_fault(
        &mut self,
        address: u32,
        len: u32,
        write: bool,
    ) -> Result<(), BusFault> {
        if !self.bus.bus_faults_armed() {
            return Ok(());
        }
        let addr = self.mask(address);
        if self.bus.take_injected_fault(addr, len, write) {
            return Err(BusFault {
                kind: BusFaultKind::BusError,
                address: addr,
            });
        }
        Ok(())
    }

    /// Report a CPU write that lands on memory already fetched as code.
    fn note_self_modifying_write(&mut self, addr: u32, len: u32) {
        let pc = self.diag_current_pc;
        let Some(smc) = self.bus.smc.as_mut() else {
            return;
        };
        if let Some(report) = smc.note_write(addr, len, pc) {
            log::warn!("smc: {}", crate::smc::SmcTracker::describe(&report));
        }
    }

    fn read_sized(&mut self, address: u32, size: usize, kind: CpuBusAccessKind) -> u32 {
        let addr = self.mask(address);
        if kind == CpuBusAccessKind::Read {
            self.bus
                .ui_note_memory_read(crate::debugger::WatchSource::Cpu, addr, size as u32);
        }
        if self.bus.heat_map_armed() {
            // Instruction fetches count as reads: seeing where the CPU is
            // executing is half of what the map is for.
            self.bus
                .note_heat(addr, size as u32, crate::heatmap::Toucher::CpuRead);
        }
        if self.icache.is_some() || self.dcache.is_some() {
            // Consume the tick-path DMA latch before any data-cache lookup:
            // under the JIT batch path a slow MMIO access mid-batch flushes
            // timed devices (whose tick can DMA into guest memory) without
            // passing the instruction-boundary coherent flush, so a driver
            // polling a completion register could otherwise hit a stale
            // line over the freshly DMA'd bytes later in the same batch.
            if kind == CpuBusAccessKind::Read
                && self.dcache.is_some()
                && self.bus.take_devices_wrote_memory()
            {
                if let Some(dcache) = self.dcache.as_deref_mut() {
                    dcache.clear_all();
                }
            }
            // 68020/030 cache models: a hit costs no bus cycle at all; a
            // miss goes through the normal (billed) path and then fills
            // the line from backing memory.
            if let Some(value) = self.cache_read(addr, size, kind) {
                if kind == CpuBusAccessKind::Fetch {
                    self.last_fetch_cache_hit = true;
                }
                return value;
            }
            let value = self.read_sized_uncached(addr, size, kind);
            self.cache_fill_after_miss(addr, size, kind);
            if kind == CpuBusAccessKind::Fetch {
                self.last_fetch_cache_hit = false;
                self.instruction_fetches_cached = false;
            }
            self.note_cpu_data_bus(addr, size, value);
            return value;
        }
        if kind == CpuBusAccessKind::Fetch {
            self.last_fetch_cache_hit = false;
            self.instruction_fetches_cached = false;
        }
        let value = self.read_sized_uncached(addr, size, kind);
        self.note_cpu_data_bus(addr, size, value);
        value
    }

    #[inline]
    fn cache_read(&self, addr: u32, size: usize, kind: CpuBusAccessKind) -> Option<u32> {
        if size > 4 {
            return None;
        }
        match kind {
            CpuBusAccessKind::Fetch => self.icache.as_deref()?.read(addr, size),
            CpuBusAccessKind::Read => self.dcache.as_deref()?.read(addr, size),
            _ => None,
        }
    }

    fn cache_fill_after_miss(&mut self, addr: u32, size: usize, kind: CpuBusAccessKind) {
        if size > 4 {
            return;
        }
        let cacheable = match kind {
            CpuBusAccessKind::Fetch => self.icache.is_some() && self.icache_cacheable(addr),
            CpuBusAccessKind::Read => self.dcache.is_some() && self.dcache_cacheable(addr),
            _ => false,
        };
        if !cacheable {
            return;
        }
        // Take the cache out so the fill closure can peek backing memory
        // through &self without a borrow conflict.
        let mut cache = match kind {
            CpuBusAccessKind::Fetch => self.icache.take(),
            CpuBusAccessKind::Read => self.dcache.take(),
            _ => None,
        };
        if let Some(cache) = cache.as_deref_mut() {
            cache.fill_after_miss(addr, size, |long| self.peek_long(long));
        }
        match kind {
            CpuBusAccessKind::Fetch => self.icache = cache,
            CpuBusAccessKind::Read => self.dcache = cache,
            _ => {}
        }
    }

    /// What the instruction cache may hold: any RAM or ROM. The boot-time
    /// overlay window is excluded so entries never alias the ROM image at
    /// chip addresses. Like real silicon, cached lines go stale if anyone
    /// (CPU or DMA) rewrites the backing memory; software clears via CACR.
    fn icache_cacheable(&self, addr: u32) -> bool {
        if self.overlay_rom_offset(addr, 1).is_some() {
            return false;
        }
        self.chip_window_offset(addr, 1).is_some()
            || self.bus.mem.zorro.region_at(addr, 1).is_some()
            || region_offset(self.bus.mem.slow_ram.len(), SLOW_RAM_BASE, addr, 1).is_some()
            || region_offset(
                self.bus.mem.mb_ram.len(),
                self.bus.mem.mb_ram_base(),
                addr,
                1,
            )
            .is_some()
            || region_offset(self.bus.mem.accel_ram.len(), ACCEL_RAM_BASE, addr, 1).is_some()
            || region_offset(self.bus.mem.rom.len(), ROM_BASE, addr, 1).is_some()
            || region_offset(
                self.bus.mem.extended_rom.len(),
                self.bus.mem.extended_rom_base,
                addr,
                1,
            )
            .is_some()
            || self.bus.cartridge.as_ref().is_some_and(|c| c.decodes(addr))
    }

    /// What the data cache may hold: expansion RAM, motherboard RAM,
    /// accelerator RAM, and ROM only. Chip and slow RAM are excluded (CIIN),
    /// as on real Amigas, because DMA writes them behind the CPU's back.
    fn dcache_cacheable(&self, addr: u32) -> bool {
        if self.overlay_rom_offset(addr, 1).is_some() {
            return false;
        }
        self.bus.mem.zorro.region_at(addr, 1).is_some()
            || region_offset(
                self.bus.mem.mb_ram.len(),
                self.bus.mem.mb_ram_base(),
                addr,
                1,
            )
            .is_some()
            || region_offset(self.bus.mem.accel_ram.len(), ACCEL_RAM_BASE, addr, 1).is_some()
            || region_offset(self.bus.mem.rom.len(), ROM_BASE, addr, 1).is_some()
            || region_offset(
                self.bus.mem.extended_rom.len(),
                self.bus.mem.extended_rom_base,
                addr,
                1,
            )
            .is_some()
            || self.bus.cartridge.as_ref().is_some_and(|c| c.decodes(addr))
    }

    fn read_sized_uncached(&mut self, address: u32, size: usize, kind: CpuBusAccessKind) -> u32 {
        let addr = self.mask(address);
        match self.classify_plain_memory(addr, size) {
            Some(PlainMemRegion::OverlayRom(off)) => {
                self.bill_external_access(size);
                return read_be(&self.bus.mem.rom, off, size);
            }
            Some(PlainMemRegion::ChipRam(off)) => {
                self.bus.grant_cpu_bus_access_at(Some(addr), size, kind);
                return read_be(&self.bus.mem.chip_ram, off, size);
            }
            Some(PlainMemRegion::ZorroRam(board, off)) => {
                // Expansion (Zorro) RAM runs at external-bus speed, off the
                // chip bus, exactly like the old fixed fast RAM mapping.
                self.bill_external_access(size);
                return read_be(self.bus.mem.zorro.board_ram(board), off, size);
            }
            Some(PlainMemRegion::MbRam(off)) => {
                // Ramsey-controlled motherboard RAM (A3000/A4000): 32-bit
                // local RAM off the chip bus, at uncontended external speed
                // like expansion RAM.
                self.bill_external_access(size);
                return read_be(&self.bus.mem.mb_ram, off, size);
            }
            Some(PlainMemRegion::AccelRam(off)) => {
                // CPU-slot (accelerator) fast RAM: local RAM on the CPU
                // board, off the chip bus, at uncontended external speed.
                self.bill_external_access(size);
                return read_be(&self.bus.mem.accel_ram, off, size);
            }
            Some(PlainMemRegion::SlowRam(off)) => {
                // "Slow"/trapdoor RAM at $C00000 is decoded by Gary and reached
                // through Agnus on the chip bus, so the CPU contends for it cycle
                // by cycle exactly like chip RAM (that is why it is "slow" and does
                // not accelerate chip-bus-bound code). Arbitrate it like chip RAM,
                // not as uncontended external memory.
                self.bus.grant_cpu_bus_access_at(Some(addr), size, kind);
                return read_be(&self.bus.mem.slow_ram, off, size);
            }
            Some(PlainMemRegion::Rom(off)) => {
                self.bill_external_access(size);
                return read_be(&self.bus.mem.rom, off, size);
            }
            // A1000 WCS at $FC0000 (empty -> no match on other machines): the
            // 256 KiB writable control store the boot ROM loads Kickstart into.
            Some(PlainMemRegion::Wcs(off)) => {
                self.bill_external_access(size);
                return read_be(&self.bus.mem.wcs, off, size);
            }
            Some(PlainMemRegion::ExtendedRom(off)) => {
                self.bill_external_access(size);
                return read_be(&self.bus.mem.extended_rom, off, size);
            }
            Some(PlainMemRegion::Cartridge(off)) => {
                // The freezer cartridge's bank: RAM on the expansion bus,
                // off the chip bus, at external-bus cost like Zorro RAM.
                self.bill_external_access(size);
                return self
                    .bus
                    .cartridge
                    .as_ref()
                    .map_or(0xFFFF_FFFF, |c| read_be(c.bank(), off, size));
            }
            None => {}
        }

        if range_contains(CIA_A_BASE, CIA_A_SIZE, addr) {
            self.bus.cpu_cia_access(Self::access_words(size));
            return self.bus.cia_a_read(u64::from(addr - CIA_A_BASE), size) as u32;
        }
        if range_contains(CIA_B_BASE, CIA_B_SIZE, addr) {
            self.bus.cpu_cia_access(Self::access_words(size));
            return self.bus.cia_b_read(u64::from(addr - CIA_B_BASE), size) as u32;
        }
        if self.bus.cdtv.is_some() && range_contains(CDTV_BATTRAM_BASE, CDTV_BATTRAM_SIZE, addr) {
            // CDTV battery-backed bookmark RAM overlays the top half of
            // the RTC page.
            self.bus.cpu_slow_external_access(Self::access_words(size));
            if let Some(cdtv) = self.bus.cdtv.as_ref() {
                return cdtv.battram_read(addr - CDTV_BATTRAM_BASE, size);
            }
        }
        if range_contains(RTC_BASE, RTC_SIZE, addr) {
            self.bus.cpu_slow_external_access(Self::access_words(size));
            return self.rtc_page_read(addr, size);
        }
        if self.bus.gayle.is_some()
            && (range_contains(GAYLE_BASE, GAYLE_SIZE, addr)
                || range_contains(GAYLE_ID_BASE, GAYLE_ID_SIZE, addr))
        {
            self.bus.cpu_slow_external_access(Self::access_words(size));
            if let Some(gayle) = self.bus.gayle.as_mut() {
                let value = gayle.read(addr, size);
                if gayle.take_activity() {
                    self.bus.note_hdd_activity();
                }
                return value;
            }
        }
        if self.bus.uaelib.is_some() && crate::uaelib::UaeLib::decodes(addr) {
            // The uaelib trap stub (crate::uaelib): ROM-like memory (WinUAE's
            // rtarea is a plain ROM bank) the CPU also fetches instructions
            // from, so it costs what the Kickstart ROM costs.
            self.bill_external_access(size);
            if let Some(uaelib) = self.bus.uaelib.as_ref() {
                return uaelib.read(addr - crate::uaelib::UAELIB_BASE, size);
            }
        }
        if size == 1 && self.bus.ramsey.is_some() && crate::ramsey::decodes(addr) {
            self.bus.cpu_slow_external_access(1);
            if let Some(ramsey) = self.bus.ramsey.as_ref() {
                return u32::from(ramsey.read_byte(addr));
            }
        }
        if size == 1 && self.bus.gary.is_some() && crate::gary::decodes(addr) {
            self.bus.cpu_slow_external_access(1);
            if let Some(gary) = self.bus.gary.as_ref() {
                return u32::from(gary.read_byte(addr));
            }
        }
        if size == 1 && self.bus.sdmac.is_some() && crate::sdmac::Sdmac::decodes(addr) {
            self.bus.cpu_slow_external_access(1);
            let value = self
                .bus
                .sdmac
                .as_mut()
                .map(|sdmac| (u32::from(sdmac.read_byte(addr)), sdmac.take_activity()));
            if let Some((value, activity)) = value {
                if activity {
                    self.bus.note_hdd_activity();
                }
                return value;
            }
        }
        if self.bus.ide_a4000.is_some() && crate::ide_a4000::IdeA4000::decodes(addr) {
            self.bus.cpu_slow_external_access(Self::access_words(size));
            if let Some(ide) = self.bus.ide_a4000.as_mut() {
                let value = ide.read(addr, size);
                if ide.take_activity() {
                    self.bus.note_hdd_activity();
                }
                return value;
            }
        }
        // A configured functional Zorro board window (registers, boot ROM, DMA
        // strobes): the chain maps it to a device slot. Off the chip bus like
        // Gayle.
        if let Some((crate::zorro::BoardBacking::Device(slot), off)) =
            self.bus.mem.zorro.device_region_at(addr, size)
        {
            self.bus.cpu_slow_external_access(Self::access_words(size));
            let (value, activity) = {
                let mem = &mut self.bus.mem;
                let dev = &mut self.bus.devices[slot];
                let mut host = crate::zorro_device::DeviceHost::for_slot(mem, slot);
                let v = crate::zorro_device::ZorroDevice::read(dev, off, size, &mut host);
                (v, crate::zorro_device::ZorroDevice::take_activity(dev))
            };
            if activity {
                self.bus.note_hdd_activity();
            }
            return value;
        }
        if self.bus.akiko.is_some()
            && range_contains(crate::akiko::AKIKO_BASE, crate::akiko::AKIKO_SIZE, addr)
        {
            self.bus.cpu_slow_external_access(Self::access_words(size));
            if let Some(akiko) = self.bus.akiko.as_mut() {
                return akiko.read(addr, size, &mut self.bus.mem);
            }
        }
        if self
            .bus
            .cdtv
            .as_ref()
            .is_some_and(|cdtv| cdtv.maps_address(addr))
        {
            self.bus.cpu_slow_external_access(Self::access_words(size));
            if let Some(cdtv) = self.bus.cdtv.as_mut() {
                // The CDTV self-decodes its 64K window; pass the window offset
                // through the ZorroDevice boundary (its read ignores memory).
                let mut host = crate::zorro_device::DeviceHost::new(&mut self.bus.mem);
                return crate::zorro_device::ZorroDevice::read(
                    cdtv,
                    addr & 0xFFFF,
                    size,
                    &mut host,
                );
            }
        }
        if let Some(off) = self.custom_reg_page_offset(addr) {
            // Custom registers live on the chip bus; custom_read itself
            // grants the contended chip-bus slot.
            return self.bus.custom_read(u64::from(off), size) as u32;
        }
        if range_contains(AUTOCONFIG_BASE as u32, AUTOCONFIG_SIZE as u32, addr) {
            self.bus.cpu_slow_external_access(Self::access_words(size));
            // The CDTV DMAC sits first in the autoconfig daisy chain.
            if let Some(cdtv) = self.bus.cdtv.as_ref() {
                if cdtv.in_config_space() {
                    return cdtv.config_read(addr, size);
                }
            }
            return self.bus.mem.zorro.config_read(u64::from(addr), size) as u32;
        }

        if size > 1 {
            let mut value = 0u32;
            for idx in 0..size {
                let byte_addr = addr.wrapping_add(idx as u32);
                // One 16-bit bus cycle covers both bytes of an aligned word.
                // If this byte lands in unmapped space, bill it only when it
                // starts a new cycle (the access's first byte, or an even
                // address continuing it); the odd second byte of the same
                // word rides the cycle already paid for. Without this, a
                // word read of unmapped space costs two full bus cycles --
                // exec's ROMTAG scan of the empty FMV/expansion window
                // ($A80000+) is a word-read loop and paid double, starting
                // the CD32 boot show seconds late.
                self.unmapped_read_continues_word = idx != 0 && byte_addr & 1 == 1;
                value = (value << 8) | self.read_sized_uncached(byte_addr, 1, kind);
            }
            self.unmapped_read_continues_word = false;
            return value;
        }

        if !self.unmapped_read_continues_word {
            // Real-CD32 probe measurement (tools/cd32-probe rows URD/ROMRD):
            // a word-read loop over the empty $A80000 expansion window runs
            // at exactly the Kickstart-ROM pace, 12.0 CPU clocks per
            // iteration -- unmapped space is auto-acknowledged at ordinary
            // external-cycle speed, not at the slower CIA/peripheral pace.
            self.bus.cpu_external_access(1);
        }
        // Undriven (unmapped) read: float to the last value the chip data bus
        // carried (display/audio DMA), as on the Agnus-arbitrated chip bus --
        // not a fixed all-ones pattern. `size` is always 1 here (size>1 recurses
        // to byte reads); a 68000 byte read takes the high data-bus byte at even
        // addresses, the low byte at odd.
        let data_bus = self.bus.data_bus;
        let value = if addr & 1 == 0 {
            u32::from(data_bus >> 8)
        } else {
            u32::from(data_bus & 0xFF)
        };
        if self
            .bus
            .log_unmapped
            .as_ref()
            .is_some_and(|r| r.contains(&addr))
        {
            log::info!("unmapped rd {addr:#08X}.b -> {value:#04X} (floating bus)");
        }
        value
    }

    /// Whether a three-byte operand at `address` lands entirely inside plain
    /// memory, and so can be moved as one sized access.
    ///
    /// This is the 68020's dynamic bus sizing decided in advance. The
    /// processor requests the whole operand and the addressed port answers
    /// with its own width: a 32-bit port (Alice chip RAM, local fast RAM)
    /// takes all three bytes in one cycle, while a 16-bit port makes the
    /// processor run a word cycle and then a byte cycle for the remainder.
    /// Plain memory is a byte array whichever width it answers at, so a
    /// single sized access carries the right data and `grant_cpu_bus_access_at`
    /// already bills it by port width. Every device port on an Amiga is 16
    /// bits, and a device decodes a register per access rather than a byte
    /// range -- one three-byte read of $DFFxxx is not a register -- so those
    /// keep the split cycles, which is both what the silicon runs and what
    /// preserves each register's own read/write side effects.
    ///
    /// A span crossing out of a region also lands here: `region_offset` only
    /// matches when the whole operand fits, so the split path handles the
    /// halves through their own decodes.
    fn three_byte_operand_is_plain_memory(&self, address: u32) -> bool {
        self.classify_plain_memory(self.mask(address), 3).is_some()
    }

    fn read_three_bytes_plain(&mut self, address: u32) -> u32 {
        self.sample_ipl();
        self.read_sized(address, 3, CpuBusAccessKind::Read)
    }

    fn write_three_bytes_plain(&mut self, address: u32, value: u32) {
        self.sample_ipl();
        self.write_sized(address, 3, value);
    }

    fn write_sized(&mut self, address: u32, size: usize, value: u32) {
        let addr = self.mask(address);
        self.note_cpu_data_bus(addr, size, value);
        if self.bus.smc.is_some() {
            self.note_self_modifying_write(addr, size as u32);
        }
        if self.bus.heat_map_armed() {
            self.bus
                .note_heat(addr, size as u32, crate::heatmap::Toucher::CpuWrite);
        }
        if let Some(dcache) = self.dcache.as_deref_mut() {
            // Write-through with invalidate-on-hit: the write itself goes
            // to memory below; later reads refill. (The instruction cache
            // intentionally does NOT snoop writes, like real silicon.)
            dcache.invalidate_write(addr, size);
        }
        match self.classify_plain_memory(addr, size) {
            Some(PlainMemRegion::OverlayRom(_)) => {
                self.bill_external_access(size);
                return;
            }
            Some(PlainMemRegion::ChipRam(off)) => {
                self.bus
                    .grant_cpu_bus_access_at(Some(addr), size, CpuBusAccessKind::Write);
                self.bus.record_cpu_chip_ram_write(off, size, value);
                self.bus.ui_note_cpu_ram_write(addr, size);
                write_be(&mut self.bus.mem.chip_ram, off, size, value);
                self.dbg_note_memw(addr, size);
                if off as u32 != addr {
                    // The write landed through an Agnus image repeat: note
                    // the canonical chip address as well as the CPU's bus
                    // address, so watchpoints and UI highlights fire
                    // whichever alias of the RAM cell they were set on.
                    self.bus.ui_note_cpu_ram_write(off as u32, size);
                    self.dbg_note_memw(off as u32, size);
                }
                return;
            }
            Some(PlainMemRegion::ZorroRam(board, off)) => {
                self.bill_external_access(size);
                write_be(self.bus.mem.zorro.board_ram_mut(board), off, size, value);
                return;
            }
            Some(PlainMemRegion::MbRam(off)) => {
                self.bill_external_access(size);
                write_be(&mut self.bus.mem.mb_ram, off, size, value);
                return;
            }
            Some(PlainMemRegion::AccelRam(off)) => {
                self.bill_external_access(size);
                write_be(&mut self.bus.mem.accel_ram, off, size, value);
                return;
            }
            Some(PlainMemRegion::SlowRam(off)) => {
                // Slow/trapdoor RAM at $C00000 is on the chip bus (reached through
                // Agnus), so CPU writes contend cycle by cycle like chip RAM rather
                // than running at uncontended external-memory speed.
                self.bus
                    .grant_cpu_bus_access_at(Some(addr), size, CpuBusAccessKind::Write);
                self.bus
                    .update_last_cpu_trace_data(u64::from(value), size.min(4) as u8);
                self.bus.ui_note_cpu_ram_write(addr, size);
                write_be(&mut self.bus.mem.slow_ram, off, size, value);
                self.dbg_note_memw(addr, size);
                return;
            }
            Some(PlainMemRegion::Rom(_)) => {
                self.bill_external_access(size);
                // A1000: a CPU write anywhere in the boot-ROM window ($F80000-
                // $FBFFFF) flips the latch to write-protect the WCS. The boot code
                // does this once the Kickstart image is in place at $FC0000.
                if !self.bus.mem.wcs.is_empty() {
                    self.bus.mem.wcs_write_protected = true;
                }
                return;
            }
            // A1000 WCS at $FC0000: writable until the boot ROM locks the latch.
            Some(PlainMemRegion::Wcs(off)) => {
                self.bill_external_access(size);
                if !self.bus.mem.wcs_write_protected {
                    write_be(&mut self.bus.mem.wcs, off, size, value);
                    self.dbg_note_memw(addr, size);
                }
                return;
            }
            Some(PlainMemRegion::ExtendedRom(_)) => {
                self.bill_external_access(size);
                return;
            }
            Some(PlainMemRegion::Cartridge(off)) => {
                self.bill_external_access(size);
                if let Some(cartridge) = self.bus.cartridge.as_mut() {
                    write_be(cartridge.bank_mut(), off, size, value);
                }
                self.dbg_note_memw(addr, size);
                return;
            }
            None => {}
        }

        if range_contains(CIA_A_BASE, CIA_A_SIZE, addr) {
            self.bus.cpu_cia_access(Self::access_words(size));
            let effect = self
                .bus
                .cia_a_write(u64::from(addr - CIA_A_BASE), size, u64::from(value));
            if effect.disable_overlay {
                // Gary's overlay flip-flop clears the moment the CIA drives
                // OVL low: the guest's very next access may already expect
                // chip RAM at $0. Flip it here rather than only at the slice
                // boundary -- the batch/JIT path retires many instructions
                // per slice, so the historical end-of-slice deferral would
                // leave the bootstrap writing its vectors into the ROM
                // mirror. The pending flag still ends the slice so the run
                // loop logs the transition.
                self.bus.mem.overlay = false;
                self.bus.overlay_disable_pending = true;
                self.bus.slice_preempted = true;
            }
            if effect.timer_started {
                self.bus.slice_preempted = true;
            }
            return;
        }
        if range_contains(CIA_B_BASE, CIA_B_SIZE, addr) {
            self.bus.cpu_cia_access(Self::access_words(size));
            let effect = self
                .bus
                .cia_b_write(u64::from(addr - CIA_B_BASE), size, u64::from(value));
            if effect.timer_started {
                self.bus.slice_preempted = true;
            }
            return;
        }
        if self.bus.cdtv.is_some() && range_contains(CDTV_BATTRAM_BASE, CDTV_BATTRAM_SIZE, addr) {
            self.bus.cpu_slow_external_access(Self::access_words(size));
            if let Some(cdtv) = self.bus.cdtv.as_mut() {
                cdtv.battram_write(addr - CDTV_BATTRAM_BASE, size, value);
            }
            return;
        }
        if range_contains(RTC_BASE, RTC_SIZE, addr) {
            self.bus.cpu_slow_external_access(Self::access_words(size));
            // An empty clock socket answers the cycle but latches nothing.
            if self.bus.rtc_present() {
                self.rtc_page_write(addr, size, value);
            }
            return;
        }
        if self.bus.gayle.is_some()
            && (range_contains(GAYLE_BASE, GAYLE_SIZE, addr)
                || range_contains(GAYLE_ID_BASE, GAYLE_ID_SIZE, addr))
        {
            self.bus.cpu_slow_external_access(Self::access_words(size));
            if let Some(gayle) = self.bus.gayle.as_mut() {
                gayle.write(addr, size, value);
                if gayle.take_activity() {
                    self.bus.note_hdd_activity();
                }
            }
            return;
        }
        if self.bus.uaelib.is_some() && crate::uaelib::UaeLib::decodes(addr) {
            // The uaelib trap doorbell (crate::uaelib): the call is serviced
            // inside this write, like a services-board DosPacket.
            self.bill_external_access(size);
            let address_mask = self.address_mask;
            let cck = self.bus.emulated_cck();
            let frame = self.bus.emulated_frames();
            let touched_memory = {
                let mem = &mut self.bus.mem;
                self.bus.uaelib.as_mut().is_some_and(|uaelib| {
                    uaelib.write(
                        addr - crate::uaelib::UAELIB_BASE,
                        size,
                        value,
                        mem,
                        address_mask,
                        cck,
                        frame,
                    )
                })
            };
            // Function 82 cleared the caller's out buffer behind the data
            // cache (the filesys packet case below does the same): drop it.
            if touched_memory {
                if let Some(dcache) = self.dcache.as_mut() {
                    dcache.clear_all();
                }
            }
            return;
        }
        if size == 1 && self.bus.gary.is_some() && crate::gary::decodes(addr) {
            self.bus.cpu_slow_external_access(1);
            if let Some(gary) = self.bus.gary.as_mut() {
                gary.write_byte(addr, value as u8);
                return;
            }
        }
        if size == 1 && self.bus.ramsey.is_some() && crate::ramsey::decodes(addr) {
            self.bus.cpu_slow_external_access(1);
            if let Some(ramsey) = self.bus.ramsey.as_mut() {
                ramsey.write_byte(addr, value as u8);
            }
            return;
        }
        if size == 1 && self.bus.sdmac.is_some() && crate::sdmac::Sdmac::decodes(addr) {
            self.bus.cpu_slow_external_access(1);
            if let Some(sdmac) = self.bus.sdmac.as_mut() {
                sdmac.write_byte(addr, value as u8);
            }
            return;
        }
        if self.bus.ide_a4000.is_some() && crate::ide_a4000::IdeA4000::decodes(addr) {
            self.bus.cpu_slow_external_access(Self::access_words(size));
            if let Some(ide) = self.bus.ide_a4000.as_mut() {
                ide.write(addr, size, value);
                if ide.take_activity() {
                    self.bus.note_hdd_activity();
                }
            }
            return;
        }
        if let Some((crate::zorro::BoardBacking::Device(slot), off)) =
            self.bus.mem.zorro.device_region_at(addr, size)
        {
            self.bus.cpu_slow_external_access(Self::access_words(size));
            let (activity, touched_memory) = {
                let mem = &mut self.bus.mem;
                let dev = &mut self.bus.devices[slot];
                let mut host = crate::zorro_device::DeviceHost::for_slot(mem, slot);
                crate::zorro_device::ZorroDevice::write(dev, off, size, value, &mut host);
                (
                    crate::zorro_device::ZorroDevice::take_activity(dev),
                    host.touched_memory(),
                )
            };
            if activity {
                self.bus.note_hdd_activity();
            }
            // A device that reached into guest memory (the filesys board
            // handling a packet) wrote behind the data cache: drop it, or
            // cached lines over the DMA'd RAM read back stale.
            if touched_memory {
                if let Some(dcache) = self.dcache.as_mut() {
                    dcache.clear_all();
                }
            }
            return;
        }
        if self.bus.akiko.is_some()
            && range_contains(crate::akiko::AKIKO_BASE, crate::akiko::AKIKO_SIZE, addr)
        {
            self.bus.cpu_slow_external_access(Self::access_words(size));
            if let Some(akiko) = self.bus.akiko.as_mut() {
                akiko.write(addr, size, value, &mut self.bus.mem);
            }
            return;
        }
        if self
            .bus
            .cdtv
            .as_ref()
            .is_some_and(|cdtv| cdtv.maps_address(addr))
        {
            self.bus.cpu_slow_external_access(Self::access_words(size));
            if let Some(mut cdtv) = self.bus.cdtv.take() {
                // Taken out to split the borrow from `mem`; routed through the
                // ZorroDevice boundary with the self-decoded window offset.
                let mut host = crate::zorro_device::DeviceHost::new(&mut self.bus.mem);
                crate::zorro_device::ZorroDevice::write(
                    &mut cdtv,
                    addr & 0xFFFF,
                    size,
                    value,
                    &mut host,
                );
                self.bus.cdtv = Some(cdtv);
            }
            return;
        }
        if let Some(off) = self.custom_reg_page_offset(addr) {
            // Custom registers live on the chip bus; custom_write itself
            // grants the contended chip-bus slot.
            if self
                .bus
                .custom_write(u64::from(off), size, u64::from(value))
            {
                self.bus.slice_preempted = true;
            }
            return;
        }
        if range_contains(AUTOCONFIG_BASE as u32, AUTOCONFIG_SIZE as u32, addr) {
            self.bus.cpu_slow_external_access(Self::access_words(size));
            // The CDTV DMAC sits first in the autoconfig daisy chain.
            if let Some(cdtv) = self.bus.cdtv.as_mut() {
                if cdtv.in_config_space() {
                    cdtv.config_write(addr, size, value);
                    return;
                }
            }
            self.bus
                .mem
                .zorro
                .config_write(u64::from(addr), size, u64::from(value));
            return;
        }

        if size > 1 {
            for idx in 0..size {
                let shift = (size - 1 - idx) * 8;
                self.write_sized(addr.wrapping_add(idx as u32), 1, (value >> shift) & 0xFF);
            }
            return;
        }
        // Dropped (unmapped) write: the 020+ posts the cycle and execution
        // continues under it. Real-CD32 probe measurement (tools/cd32-probe
        // row UWR): a word-write loop over $A80000 runs at 8.0 CPU clocks
        // per iteration, the same as the posted chip-write cadence -- two
        // clocks less than the core's nominal charge, so credit that
        // overlap here. TODO: back-to-back streams (no loop overhead) may
        // run slightly faster than a real bus-limited write stream; a
        // dedicated probe row would pin the streaming case.
        self.bus.credit_cpu_posted_void_write();
        if self
            .bus
            .log_unmapped
            .as_ref()
            .is_some_and(|r| r.contains(&addr))
        {
            log::info!("unmapped wr {addr:#08X}.b <- {value:#04X} (dropped)");
        }
    }

    fn overlay_rom_offset(&self, addr: u32, size: usize) -> Option<usize> {
        if !self.bus.mem.overlay {
            return None;
        }
        region_offset(self.bus.mem.rom.len(), CHIP_RAM_BASE, addr, size)
    }
}

impl CpuBus {
    /// Latch the IPL level presented to the CPU by (the end of) a bus access.
    /// The real 68000 samples its IPL pins during bus cycles, and the
    /// take-interrupt decision at an instruction boundary uses the value
    /// latched during the previous instruction's last bus access -- a level
    /// that rises during an instruction's trailing internal cycles is not
    /// seen until one instruction later (Moira models the same points as
    /// POLL_IPL). `refresh_irq_line` consumes this latch.
    #[inline]
    fn sample_ipl(&mut self) {
        // A poll-point hold (AddressBus::ipl_hold_sample) pins the sample
        // taken at the instruction's microcode poll access; accesses after
        // the poll point (e.g. an RMW writeback) do not re-latch.
        if self.ipl_sample_held {
            return;
        }
        self.sampled_irq_level = if self.bus.cartridge_nmi_pending() {
            7
        } else if self.bus.paula.intena & INT_MASTER != 0 {
            pending_ipl(self.bus.paula.intena & self.bus.cpu_visible_intreq())
        } else {
            0
        };
    }

    /// The zero-cost guest-RAM window offered to the core's batch/JIT path
    /// (`AddressBus::fast_mem`): the largest fast-RAM bank fitted -- a
    /// configured Zorro board, Ramsey motherboard RAM, or CPU-slot
    /// accelerator RAM. Chip and slow RAM are never offered: their CPU
    /// accesses arbitrate into chip-bus DMA slots and drive the shared data
    /// bus, side effects the window contract forbids.
    ///
    /// Returns None while anything that intercepts plain fast-RAM accesses
    /// is armed (cache models, memory-write debug hooks, the heat map, SMC
    /// detection, injected bus faults) so those features keep seeing every
    /// access; the batch path then simply routes everything through the
    /// normal bus methods. Recomputed on every `run_batch` call, so a bank
    /// that autoconfig places (or a RESET tears down) is picked up at the
    /// next batch boundary.
    ///
    /// A 68000/68010 never gets a window at all: on the small-box machines
    /// every CPU cycle -- fast RAM included -- drives the one bus shared
    /// with Agnus, and `note_cpu_data_bus` must latch each transfer so
    /// undriven (unmapped) reads float to the last driven value. Kickstart
    /// and AROS memory sizing read exactly those floats, so bypassing the
    /// latch wedges the boot. The 020+ run fast RAM on their own local
    /// bus (`cpu_short_bus_cycle`), where the latch is deliberately not
    /// driven and the window is side-effect free.
    fn jit_fast_mem(&mut self) -> Option<FastMem> {
        if !self.jit_enabled
            || !self.bus.cpu_short_bus_cycle()
            || self.icache.is_some()
            || self.dcache.is_some()
            || self.dbg_memw_addr.is_some()
            || self.bus.heat_map_armed()
            || self.bus.smc.is_some()
            || self.bus.bus_faults_armed()
        {
            return None;
        }
        let zorro = self.bus.mem.zorro.largest_ram_region();
        let mb_len = self.bus.mem.mb_ram.len() as u32;
        let accel_len = self.bus.mem.accel_ram.len() as u32;
        let zorro_len = zorro.map_or(0, |(_, len, _)| len);
        let best = zorro_len.max(mb_len).max(accel_len);
        if best == 0 {
            return None;
        }
        if best == zorro_len {
            let (base, len, idx) = zorro?;
            let ram = self.bus.mem.zorro.board_ram_mut(idx);
            return Some(FastMem {
                ptr: ram.as_mut_ptr(),
                base,
                len,
            });
        }
        if best == mb_len {
            let base = self.bus.mem.mb_ram_base() as u32;
            return Some(FastMem {
                ptr: self.bus.mem.mb_ram.as_mut_ptr(),
                base,
                len: mb_len,
            });
        }
        Some(FastMem {
            ptr: self.bus.mem.accel_ram.as_mut_ptr(),
            base: ACCEL_RAM_BASE as u32,
            len: accel_len,
        })
    }
}

impl AddressBus for CpuBus {
    fn last_fetch_was_cached(&self) -> bool {
        self.last_fetch_cache_hit
    }

    fn fast_mem(&mut self) -> Option<FastMem> {
        self.jit_fast_mem()
    }

    fn begin_instruction_fetches(&mut self) {
        self.instruction_fetches_cached = true;
    }

    fn instruction_fetches_were_cached(&self) -> bool {
        self.instruction_fetches_cached
    }

    fn try_read_byte(&mut self, address: u32) -> Result<u8, BusFault> {
        self.check_injected_fault(address, 1, false)?;
        Ok(self.read_byte(address))
    }

    fn try_read_word(&mut self, address: u32) -> Result<u16, BusFault> {
        self.check_injected_fault(address, 2, false)?;
        Ok(self.read_word(address))
    }

    fn try_read_long(&mut self, address: u32) -> Result<u32, BusFault> {
        self.check_injected_fault(address, 4, false)?;
        Ok(self.read_long(address))
    }

    fn try_write_byte(&mut self, address: u32, value: u8) -> Result<(), BusFault> {
        self.check_injected_fault(address, 1, true)?;
        self.write_byte(address, value);
        Ok(())
    }

    fn try_write_word(&mut self, address: u32, value: u16) -> Result<(), BusFault> {
        self.check_injected_fault(address, 2, true)?;
        self.write_word(address, value);
        Ok(())
    }

    fn try_write_long(&mut self, address: u32, value: u32) -> Result<(), BusFault> {
        self.check_injected_fault(address, 4, true)?;
        self.write_long(address, value);
        Ok(())
    }

    fn try_read_three_bytes(&mut self, address: u32) -> Result<u32, BusFault> {
        if !self.three_byte_operand_is_plain_memory(address) {
            let hi = self.try_read_word(address)? as u32;
            let lo = self.try_read_byte(address.wrapping_add(2))? as u32;
            return Ok((hi << 8) | lo);
        }
        self.check_injected_fault(address, 3, false)?;
        Ok(self.read_three_bytes_plain(address))
    }

    fn try_write_three_bytes(&mut self, address: u32, value: u32) -> Result<(), BusFault> {
        if !self.three_byte_operand_is_plain_memory(address) {
            self.try_write_word(address, (value >> 8) as u16)?;
            return self.try_write_byte(address.wrapping_add(2), value as u8);
        }
        self.check_injected_fault(address, 3, true)?;
        self.write_three_bytes_plain(address, value);
        Ok(())
    }

    fn read_three_bytes(&mut self, address: u32) -> u32 {
        if !self.three_byte_operand_is_plain_memory(address) {
            let hi = self.read_word(address) as u32;
            let lo = self.read_byte(address.wrapping_add(2)) as u32;
            return (hi << 8) | lo;
        }
        self.read_three_bytes_plain(address)
    }

    fn write_three_bytes(&mut self, address: u32, value: u32) {
        if !self.three_byte_operand_is_plain_memory(address) {
            self.write_word(address, (value >> 8) as u16);
            self.write_byte(address.wrapping_add(2), value as u8);
            return;
        }
        self.write_three_bytes_plain(address, value);
    }

    fn try_read_immediate_word(&mut self, address: u32) -> Result<u16, BusFault> {
        self.check_injected_fault(address, 2, false)?;
        Ok(self.read_immediate_word(address))
    }

    fn try_read_immediate_long(&mut self, address: u32) -> Result<u32, BusFault> {
        self.check_injected_fault(address, 4, false)?;
        Ok(self.read_immediate_long(address))
    }

    fn read_byte(&mut self, address: u32) -> u8 {
        self.sample_ipl();
        self.read_sized(address, 1, CpuBusAccessKind::Read) as u8
    }

    fn read_word(&mut self, address: u32) -> u16 {
        self.sample_ipl();
        self.read_sized(address, 2, CpuBusAccessKind::Read) as u16
    }

    fn read_long(&mut self, address: u32) -> u32 {
        self.sample_ipl();
        self.read_sized(address, 4, CpuBusAccessKind::Read)
    }

    fn write_byte(&mut self, address: u32, value: u8) {
        self.sample_ipl();
        self.write_sized(address, 1, u32::from(value));
    }

    fn write_word(&mut self, address: u32, value: u16) {
        self.sample_ipl();
        self.write_sized(address, 2, u32::from(value));
    }

    fn write_long(&mut self, address: u32, value: u32) {
        self.sample_ipl();
        self.write_sized(address, 4, value);
    }

    fn read_immediate_word(&mut self, address: u32) -> u16 {
        self.sample_ipl();
        self.read_sized(address, 2, CpuBusAccessKind::Fetch) as u16
    }

    fn read_immediate_long(&mut self, address: u32) -> u32 {
        self.sample_ipl();
        self.read_sized(address, 4, CpuBusAccessKind::Fetch)
    }

    fn sync(&mut self, cpu_clocks: u32) {
        // Cycle-exact core (Part E.2): internal (non-bus) CPU clocks elapse
        // with the chip bus free for DMA, timed devices ticking along.
        if diag_cpu_sync_on() && diag_cpu_sync_pc_matches(self.diag_current_pc) {
            eprintln!(
                "CPUSYNC preaccess pc={:08x} clocks={} v={:03x} h={:02x}",
                self.diag_current_pc, cpu_clocks, self.bus.agnus.vpos, self.bus.agnus.hpos,
            );
        }
        self.bus.sync_cpu_internal_clocks(cpu_clocks);
    }

    fn ipl_hold_sample(&mut self) {
        // Core-marked IPL poll point: keep the sample latched by the access
        // that just completed until the boundary decision consumes it.
        self.ipl_sample_held = true;
    }

    fn ipl_release_sample(&mut self) {
        // Exception dispatch: the handler-entry refill is a fresh poll
        // point, so drop any hold from the faulted instruction.
        self.ipl_sample_held = false;
    }

    fn interrupt_acknowledge(&mut self, level: u8) -> u32 {
        // A freezer cartridge's level-7 interrupt: consumed at its
        // acknowledge, so the monitor is entered exactly once per press
        // (the 68000 recognises level 7 on its transition). The vector the
        // freeze wrote may sit under a stale data-cache line when VBR
        // points into fast RAM, so the cache is dropped before the core
        // fetches it.
        if level == 7 && self.bus.cartridge_take_nmi() {
            if let Some(dcache) = self.dcache.as_deref_mut() {
                dcache.clear_all();
            }
            trace!("freezer cartridge NMI acknowledged");
            return AUTOVECTOR_SENTINEL;
        }
        let pending = self.bus.paula.intena & self.bus.cpu_visible_intreq();
        self.bus.delivered_irq_pending = pending;
        if pending & crate::chipset::paula::INT_COPER != 0 {
            self.bus.delivered_copper_irq_beam = self.bus.pending_copper_irq_beam;
            self.bus.pending_copper_irq_beam = None;
        }
        trace!(
            "autovector IRQ level {} pending={:#06X} intreq={:#06X} intena={:#06X}",
            level,
            pending,
            self.bus.paula.intreq,
            self.bus.paula.intena
        );
        // COPPERLINE_DBG_IRQ: log every serviced interrupt (level + the enabled
        // pending source bits) with emulated time, to measure handler rates
        // (VERTB=0x20, BLIT=0x40, SOFT=0x04, COPER=0x10, PORTS=0x08, EXTER=0x2000).
        // Bounded by COPPERLINE_DBG_AFTER/UNTIL.
        if let Some((after, until)) = self.dbg_irq_window {
            let secs = self.bus.emulated_seconds();
            if secs >= after && secs < until {
                log::info!(
                    "irq lvl={level} pending={pending:#06X} secs={secs:.5} f={}",
                    self.bus.emulated_frames(),
                );
            }
        }
        AUTOVECTOR_SENTINEL
    }

    fn reset_devices(&mut self) {
        self.bus.reset_custom_chips_from_cpu_reset();
    }
}

/// Adapts the live CPU core and bus to the [`BreakContext`] a breakpoint
/// condition reads. Borrows the `cpu` and `bus` fields disjointly from the
/// breakpoint set so the gate can update hit counters while evaluating.
struct MachineBreakContext<'a> {
    cpu: &'a CpuCore,
    bus: &'a crate::bus::Bus,
}

impl crate::debugger::BreakContext for MachineBreakContext<'_> {
    fn data(&self, n: usize) -> u32 {
        self.cpu.d(n)
    }
    fn addr_reg(&self, n: usize) -> u32 {
        self.cpu.a(n)
    }
    fn pc(&self) -> u32 {
        self.cpu.pc
    }
    fn sr(&self) -> u32 {
        u32::from(self.cpu.get_sr())
    }
    fn mem_word(&self, addr: u32) -> u16 {
        self.bus.peek_word_any(addr)
    }
}

fn cpu_type_for_model(model: CpuModel) -> CpuType {
    match model {
        CpuModel::M68000 => CpuType::M68000,
        CpuModel::M68010 => CpuType::M68010,
        CpuModel::M68EC020 => CpuType::M68EC020,
        CpuModel::M68020 => CpuType::M68020,
        CpuModel::M68030 => CpuType::M68030,
        // Full 68040: FPU on-die (default_fpu) and the MMU enabled (has_pmmu).
        // The FPU-less LC/EC variants are not modelled (see CpuModel docs).
        CpuModel::M68040 => CpuType::M68040,
        CpuModel::M68060 => CpuType::M68060,
    }
}

/// Longword entries in each on-chip cache for this CPU: the 060's caches
/// are 8 KB (2048 longwords), the 040's 4 KB, the 020/030's 256 bytes.
/// See `crate::cache`.
fn cache_lines_for_cpu_type(cpu_type: CpuType) -> usize {
    match cpu_type {
        CpuType::M68060 => crate::cache::LINES_060,
        CpuType::M68EC040 | CpuType::M68LC040 | CpuType::M68040 => crate::cache::LINES_040,
        _ => crate::cache::LINES_020,
    }
}

fn address_mask_for_model(model: CpuModel) -> u32 {
    match model {
        CpuModel::M68000 | CpuModel::M68010 | CpuModel::M68EC020 => ADDRESS_MASK_24BIT,
        CpuModel::M68020 | CpuModel::M68030 | CpuModel::M68040 | CpuModel::M68060 => {
            ADDRESS_MASK_32BIT
        }
    }
}

fn positive_cpu_cycles(cycles: i32) -> u32 {
    cycles.max(0) as u32
}

fn debug_irq_window_setting() -> Option<(f64, f64)> {
    if !crate::envcfg::flag("COPPERLINE_DBG_IRQ") {
        return None;
    }
    let after = crate::envcfg::var("COPPERLINE_DBG_AFTER")
        .and_then(|s| s.trim().parse::<f64>().ok())
        .unwrap_or(0.0);
    let until = crate::envcfg::var("COPPERLINE_DBG_UNTIL")
        .and_then(|s| s.trim().parse::<f64>().ok())
        .unwrap_or(f64::INFINITY);
    Some((after, until))
}

fn is_fpu_instruction_family(opword: u16) -> bool {
    matches!(opword & 0xFE00, 0xF200)
}

fn range_contains(base: u32, size: u32, address: u32) -> bool {
    address.wrapping_sub(base) < size
}

fn region_offset(len: usize, base: u64, address: u32, size: usize) -> Option<usize> {
    if len == 0 || size == 0 {
        return None;
    }
    let addr = u64::from(address);
    let end = addr.checked_add(size as u64)?;
    let base_end = base.checked_add(len as u64)?;
    if addr >= base && end <= base_end {
        Some((addr - base) as usize)
    } else {
        None
    }
}

fn read_be(bytes: &[u8], off: usize, size: usize) -> u32 {
    let mut value = 0u32;
    for byte in &bytes[off..off + size] {
        value = (value << 8) | u32::from(*byte);
    }
    value
}

fn write_be(bytes: &mut [u8], off: usize, size: usize, value: u32) {
    for idx in 0..size {
        let shift = (size - 1 - idx) * 8;
        bytes[off + idx] = ((value >> shift) & 0xFF) as u8;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn charge_cpu_clocks_scales_with_clock_ratio_and_carries_remainders() -> Result<()> {
        let machine_at = |clocks_per_cck: u32| -> Result<M68kMachine> {
            build(
                test_bus(reset_rom(0, 0)),
                CpuModel::M68000,
                false,
                clocks_per_cck,
                Default::default(),
                false,
            )
        };

        // Stock 68000: 2 clocks per CCK -> halve.
        let mut machine = machine_at(2)?;
        assert_eq!(machine.charge_cpu_clocks(8), 4);
        // 020 at 4x: a quarter of the colour clocks per CPU cycle.
        let mut machine = machine_at(4)?;
        assert_eq!(machine.charge_cpu_clocks(8), 2);
        // Sub-cck instructions accumulate via the carry instead of rounding
        // each one up to a whole cck: 7 clocks at 28/cck yields one cck on
        // every fourth call.
        let mut machine = machine_at(28)?;
        let total: u32 = (0..8).map(|_| machine.charge_cpu_clocks(7)).sum();
        assert_eq!(total, 2);
        // Negative cycle reports are clamped to zero.
        let mut machine = machine_at(2)?;
        assert_eq!(machine.charge_cpu_clocks(-4), 0);
        Ok(())
    }

    use crate::audio::NullSink;
    use crate::chipset::cia::{REG_CRA, REG_DDRA, REG_ICR, REG_PRA, REG_TAHI, REG_TALO};
    use crate::chipset::copper::DMACON_COPEN;
    use crate::chipset::paula::Paula;
    use crate::chipset::paula::{DMACON_DMAEN, INT_COPER, INT_MASTER, INT_PORTS, INT_VERTB};
    use crate::floppy::FloppyController;
    use crate::memory::{Memory, FAST_RAM_BASE, ROM_SIZE};
    use crate::serial::StdoutSink;
    use crate::zorro::{BoardSpec, ZorroChain};

    fn test_bus(rom: Vec<u8>) -> Bus {
        let mut chip_ram = vec![0; 512 * 1024];
        let seed = rom.len().min(chip_ram.len());
        chip_ram[..seed].copy_from_slice(&rom[..seed]);
        let mut zorro = ZorroChain::default();
        zorro
            .add_board_configured_at(BoardSpec::fast_ram(64 * 1024), FAST_RAM_BASE as u32)
            .unwrap();
        Bus::new(
            Memory {
                chip_ram,
                slow_ram: vec![0; 64 * 1024],
                mb_ram: Vec::new(),
                accel_ram: Vec::new(),
                rom,
                overlay: false,
                zorro,
                extended_rom: Vec::new(),
                extended_rom_base: 0,
                wcs: Vec::new(),
                wcs_write_protected: false,
            },
            Paula::new(Box::new(StdoutSink::new()), Box::new(NullSink)),
            FloppyController::default(),
        )
    }

    fn test_bus_with_pc(pc: u32) -> Bus {
        test_bus(reset_rom(0x0007_FFFE, pc))
    }

    fn reset_rom(sp: u32, pc: u32) -> Vec<u8> {
        let mut rom = vec![0; ROM_SIZE];
        rom[0..4].copy_from_slice(&sp.to_be_bytes());
        rom[4..8].copy_from_slice(&pc.to_be_bytes());
        rom
    }

    fn write_words(bytes: &mut [u8], off: usize, words: &[u16]) {
        for (idx, word) in words.iter().enumerate() {
            let off = off + idx * 2;
            bytes[off..off + 2].copy_from_slice(&word.to_be_bytes());
        }
    }

    fn write_program(bus: &mut Bus, address: u32, words: &[u16]) {
        let addr = u64::from(address);
        if let Some(off) = region_offset(bus.mem.chip_ram.len(), CHIP_RAM_BASE, address, 1) {
            write_words(&mut bus.mem.chip_ram, off, words);
        } else if let Some((board, off)) = bus.mem.zorro.region_at(address, 1) {
            write_words(bus.mem.zorro.board_ram_mut(board), off, words);
        } else if let Some(off) = region_offset(bus.mem.slow_ram.len(), SLOW_RAM_BASE, address, 1) {
            write_words(&mut bus.mem.slow_ram, off, words);
        } else if let Some(off) = addr.checked_sub(ROM_BASE).and_then(|off| {
            (off as usize + words.len() * 2 <= bus.mem.rom.len()).then_some(off as usize)
        }) {
            write_words(&mut bus.mem.rom, off, words);
        } else {
            panic!("test program address {address:#08X} is not writable in the fixture");
        }
    }

    fn write_chip_long(bus: &mut Bus, address: u32, value: u32) {
        let off = region_offset(bus.mem.chip_ram.len(), CHIP_RAM_BASE, address, 4)
            .expect("test chip long address must fit chip RAM");
        bus.mem.chip_ram[off..off + 4].copy_from_slice(&value.to_be_bytes());
    }

    /// Ramsey motherboard RAM decodes at the top of its window (ending at
    /// $08000000) for a 32-bit CPU: stores land in the bank and read back
    /// through the same decode. The bank sits beyond a 24-bit address bus,
    /// so the same access on a 68000 wraps into the low 16 MB and must
    /// leave the fitted RAM untouched.
    #[test]
    fn motherboard_ram_decodes_at_the_top_of_its_window() -> Result<()> {
        let program = [
            0x23FCu16, 0xCAFE, 0xBABE, 0x07FF, 0xFFFC, // move.l #$CAFEBABE,$07FFFFFC
            0x2039, 0x07FF, 0xFFFC, // move.l $07FFFFFC,d0
            0x23C0, 0x0000, 0x1000, // move.l d0,$1000
            0x60FE, // bra.s *
        ];

        let mut bus = test_bus_with_pc(0x00F8_0100);
        bus.mem.fit_mb_ram(2 * 1024 * 1024);
        assert_eq!(bus.mem.mb_ram_base(), 0x07E0_0000);
        write_program(&mut bus, 0x00F8_0100, &program);
        let mut machine = build(bus, CpuModel::M68030, false, 2, Default::default(), false)?;
        machine.step_slice(3)?;
        let mem = &machine.bus.bus.mem;
        let top = mem.mb_ram.len();
        assert_eq!(&mem.mb_ram[top - 4..], &0xCAFE_BABEu32.to_be_bytes());
        // The readback flowed through d0 into chip RAM.
        assert_eq!(&mem.chip_ram[0x1000..0x1004], &0xCAFE_BABEu32.to_be_bytes());
        // The debugger's any-region peek sees the bank at its bus address.
        assert_eq!(machine.bus.bus.peek_word_any(0x07FF_FFFC), 0xCAFE);

        let mut bus = test_bus_with_pc(0x00F8_0100);
        bus.mem.fit_mb_ram(2 * 1024 * 1024);
        write_program(&mut bus, 0x00F8_0100, &program);
        let mut machine = build(bus, CpuModel::M68000, false, 2, Default::default(), false)?;
        machine.step_slice(3)?;
        let mem = &machine.bus.bus.mem;
        let top = mem.mb_ram.len();
        assert_eq!(&mem.mb_ram[top - 4..], &[0, 0, 0, 0]);
        Ok(())
    }

    /// CPU-slot (accelerator) RAM decodes from the bottom of the
    /// coprocessor-slot space at $08000000 for a 32-bit CPU: stores land in
    /// the bank and read back through the same decode. Like the motherboard
    /// bank, it sits beyond a 24-bit address bus, so the same access on a
    /// 68000 wraps into the low 16 MB and must leave the fitted RAM
    /// untouched.
    #[test]
    fn accelerator_ram_decodes_at_the_bottom_of_the_cpu_slot_space() -> Result<()> {
        let program = [
            0x23FCu16, 0xCAFE, 0xBABE, 0x0800, 0x0000, // move.l #$CAFEBABE,$08000000
            0x2039, 0x0800, 0x0000, // move.l $08000000,d0
            0x23C0, 0x0000, 0x1000, // move.l d0,$1000
            0x60FE, // bra.s *
        ];

        let mut bus = test_bus_with_pc(0x00F8_0100);
        bus.mem.fit_accel_ram(2 * 1024 * 1024);
        write_program(&mut bus, 0x00F8_0100, &program);
        let mut machine = build(bus, CpuModel::M68030, false, 2, Default::default(), false)?;
        machine.step_slice(3)?;
        let mem = &machine.bus.bus.mem;
        assert_eq!(&mem.accel_ram[..4], &0xCAFE_BABEu32.to_be_bytes());
        // The readback flowed through d0 into chip RAM.
        assert_eq!(&mem.chip_ram[0x1000..0x1004], &0xCAFE_BABEu32.to_be_bytes());
        // The debugger's any-region peek sees the bank at its bus address.
        assert_eq!(machine.bus.bus.peek_word_any(0x0800_0000), 0xCAFE);

        let mut bus = test_bus_with_pc(0x00F8_0100);
        bus.mem.fit_accel_ram(2 * 1024 * 1024);
        write_program(&mut bus, 0x00F8_0100, &program);
        let mut machine = build(bus, CpuModel::M68000, false, 2, Default::default(), false)?;
        machine.step_slice(3)?;
        let mem = &machine.bus.bus.mem;
        assert_eq!(&mem.accel_ram[..4], &[0, 0, 0, 0]);
        Ok(())
    }

    fn write_chip_word(bus: &mut Bus, address: u32, value: u16) {
        let off = region_offset(bus.mem.chip_ram.len(), CHIP_RAM_BASE, address, 2)
            .expect("test chip word address must fit chip RAM");
        bus.mem.chip_ram[off..off + 2].copy_from_slice(&value.to_be_bytes());
    }

    fn read_chip_word(bus: &Bus, address: u32) -> u16 {
        let off = region_offset(bus.mem.chip_ram.len(), CHIP_RAM_BASE, address, 2)
            .expect("test chip word address must fit chip RAM");
        u16::from_be_bytes([bus.mem.chip_ram[off], bus.mem.chip_ram[off + 1]])
    }

    fn read_chip_long(bus: &Bus, address: u32) -> u32 {
        let off = region_offset(bus.mem.chip_ram.len(), CHIP_RAM_BASE, address, 4)
            .expect("test chip long address must fit chip RAM");
        u32::from_be_bytes([
            bus.mem.chip_ram[off],
            bus.mem.chip_ram[off + 1],
            bus.mem.chip_ram[off + 2],
            bus.mem.chip_ram[off + 3],
        ])
    }

    fn set_autovector(bus: &mut Bus, level: u8, handler: u32) {
        let vector_addr = 0x60 + u32::from(level) * 4;
        write_chip_long(bus, vector_addr, handler);
    }

    fn machine_with_program(pc: u32, words: &[u16]) -> Result<M68kMachine> {
        machine_with_program_model(pc, words, CpuModel::M68000)
    }

    #[test]
    fn debugger_pc_redirect_discards_the_old_instruction_prefetch() -> Result<()> {
        let mut machine = machine_with_program(0x100, &[0x4e71, 0x7001, 0x4e71])?;
        write_program(machine.bus_mut(), 0x200, &[0x7002, 0x4e71]);
        machine.step_slice(1)?; // NOP prefetches MOVEQ #1 at the old PC.
        assert!(machine.debug_set_register(17, 0x200));
        machine.step_slice(1)?;
        assert_eq!(
            machine.d(0),
            2,
            "forced PC must fetch the new target opcode"
        );
        assert_eq!(machine.pc(), 0x202);
        Ok(())
    }

    // Replicate the timing-test ROM's row 10/11/18 inner loop through the real
    // CPU + bus so the bitplane-contention over-charge can be localised. The
    // loop is `move.w d1,(a0) ; dbra d6,loop` running from
    // chip RAM (code-fetch contends) and writing a chip-RAM framebuffer, exactly
    // like the ROM. Reports the contention delta (DMA on minus DMA off) per
    // iteration for 6-plane and 3-plane lores, and a per-line cck breakdown.
    #[test]
    fn audit_realistic_loop_contention() {
        const BPLEN: u16 = 0x0100;
        // move.w d1,(a0) = 0x3081 ; dbra d6,loop = 0x51CE,0xFFFC (-4)
        let prog = [0x3081u16, 0x51CE, 0xFFFC];
        let loop_pc = 0x0003_0000u32;
        let iters = 1200u32;

        // Run the loop, returning (total bus cck, per-line cck buckets keyed by vpos).
        let run = |planes: u16| -> u32 {
            let mut machine = machine_with_program(loop_pc, &prog).unwrap();
            machine.cpu.set_a(0, 0x0002_0000); // framebuffer, away from the code
            machine.cpu.set_d(1, 0x1234);
            machine.cpu.set_d(6, iters - 1);
            {
                let bus = machine.bus_mut();
                bus.denise.diwstrt = 0x2C81; // vstart 0x2C = 44
                bus.denise.diwstop = 0x2CC1; // vstop 0x12C = 300
                bus.denise.ddfstrt = 0x0038;
                bus.denise.ddfstop = 0x00D0;
                bus.denise.bplcon0 = planes << 12; // BPU field, lores
                bus.agnus.vpos = 0x40; // inside the vertical display window
                bus.agnus.hpos = 0;
                bus.agnus.dmacon = if planes == 0 {
                    DMACON_DMAEN
                } else {
                    DMACON_DMAEN | BPLEN
                };
            }
            let mut total = 0u32;
            // 2 instructions (move + dbra) per loop iteration.
            for _ in 0..(2 * iters) {
                total += machine.step_slice(1).unwrap().bus_advanced_cck;
            }
            total
        };

        let no_dma = run(0);
        let six = run(6);
        let three = run(3);
        let it = iters as f64;
        // Reference (timing-test ROM, cck = E-ticks * 5 / 1024 iters):
        //   row 10 (no DMA):  ~9.10 cck/iter (both Copperline and FS-UAE)
        //   row 11 (6-plane): FS-UAE 13.14, Copperline-ROM 13.20 cck/iter
        // This isolated harness reproduces ~9.08 (row 10) and ~13.15 (row 11),
        // i.e. the core bitplane-contention model already matches FS-UAE to
        // within measurement noise. The extra ~0.05 the full ROM run shows comes
        // from the surrounding frame structure, not the slot model.
        eprintln!("=== realistic move.w+dbra loop, {iters} iters ===");
        eprintln!("no_dma  = {no_dma} ({:.4} cck/iter)", no_dma as f64 / it);
        eprintln!(
            "6-plane = {six} (delta {}, {:.4} cck/iter; total {:.4} cck/iter)",
            six - no_dma,
            (six - no_dma) as f64 / it,
            six as f64 / it
        );
        eprintln!(
            "3-plane = {three} (delta {}, {:.4} cck/iter; total {:.4} cck/iter)",
            three - no_dma,
            (three - no_dma) as f64 / it,
            three as f64 / it
        );
    }

    fn machine_with_program_model(pc: u32, words: &[u16], model: CpuModel) -> Result<M68kMachine> {
        let mut bus = test_bus_with_pc(pc);
        write_program(&mut bus, pc, words);
        let mut machine = M68kMachine::new(bus, model, false)?;
        machine.bus_mut().agnus.hpos = 0x21;
        Ok(machine)
    }

    fn run_one_instruction_at(pc: u32, words: &[u16]) -> Result<CpuStepSlice> {
        let mut machine = machine_with_program(pc, words)?;
        machine.step_slice(1)
    }

    fn run_rom_instruction(words: &[u16]) -> Result<CpuStepSlice> {
        run_one_instruction_at(ROM_BASE as u32 + 0x0100, words)
    }

    // Long-indexed lookup: `move.w (a1,d0.l),d1` must read the
    // word at a1+d0, not a1+d0/2 or a1+2*d0. d0=0xA -> word at $80A.
    #[test]
    fn move_w_long_indexed_reads_correct_table_entry() -> Result<()> {
        // moveq #$0A,d0 ; lea $800,a1 ; move.w (a1,d0.l),d1
        let mut machine =
            machine_with_program(0x0500, &[0x700A, 0x43F9, 0x0000, 0x0800, 0x3231, 0x0800])?;
        {
            let cr = &mut machine.bus_mut().mem.chip_ram;
            for off in (0x800..0x820).step_by(2) {
                cr[off..off + 2].copy_from_slice(&0xDEADu16.to_be_bytes());
            }
            cr[0x808..0x80A].copy_from_slice(&0x1111u16.to_be_bytes());
            cr[0x80A..0x80C].copy_from_slice(&0xBEEFu16.to_be_bytes());
            cr[0x80C..0x80E].copy_from_slice(&0x2222u16.to_be_bytes());
        }
        machine.step_slice(3)?;
        assert_eq!(
            machine.d(1) & 0xFFFF,
            0xBEEF,
            "move.w (a1,d0.l),d1 must read the word at a1+d0"
        );
        Ok(())
    }

    #[test]
    fn m68ec020_masks_external_addresses_to_24_bits() -> Result<()> {
        // MOVEA retains all 32 register bits, but the EC020 exposes only
        // A0-A23 externally. The MOVE.W must therefore read $000800 when A0
        // contains $01000800 rather than issuing an unmapped 32-bit access.
        let mut machine = machine_with_program_model(
            0x0500,
            &[
                0x207C, 0x0100, 0x0800, // movea.l #$01000800,a0
                0x3010, // move.w (a0),d0
            ],
            CpuModel::M68EC020,
        )?;
        write_chip_word(machine.bus_mut(), 0x0800, 0xBEEF);

        machine.step_slice(2)?;

        assert_eq!(machine.a(0), 0x0100_0800, "address register stays 32-bit");
        assert_eq!(
            machine.d(0) & 0xFFFF,
            0xBEEF,
            "external address wraps at 24 bits"
        );
        Ok(())
    }

    // The motherboard decode routes the whole $000000-$1FFFFF window to
    // Agnus, which decodes only as many address bits as its DRAM reach, so
    // the fitted chip RAM image repeats across the window (every 512 KiB on
    // OCS). Regression example: Action Replay freeze-disk loaders point the
    // supervisor stack at $100000 on a 512 KiB machine and rely on the
    // pushes landing at the top of chip RAM.
    #[test]
    fn cpu_chip_window_image_aliases_fitted_ram() -> Result<()> {
        // move.l #$DEADBEEF,$180010.l ; move.w $100012.l,d1
        let mut machine = machine_with_program(
            0x0500,
            &[
                0x23FC, 0xDEAD, 0xBEEF, 0x0018, 0x0010, // move.l #imm,$180010.l
                0x3239, 0x0010, 0x0012, // move.w $100012.l,d1
            ],
        )?;
        // Watch both aliases of the target cell: the mirror write must fire
        // the watch whichever alias it was set on.
        machine.bus_mut().set_ui_mem_watches(&[0x10, 0x180010]);
        machine.step_slice(2)?;
        // On the fixture's OCS Agnus both $180010 and $100012 are images of
        // chip $10/$12.
        assert_eq!(read_chip_long(machine.bus(), 0x10), 0xDEADBEEF);
        assert_eq!(machine.d(1) & 0xFFFF, 0xBEEF);
        assert!(machine.bus_mut().ui_take_mem_writer(0x10).is_some());
        assert!(machine.bus_mut().ui_take_mem_writer(0x180010).is_some());
        Ok(())
    }

    #[test]
    fn chip_window_images_follow_agnus_reach() -> Result<()> {
        use crate::chipset::agnus::AgnusRevision;
        let mut machine = machine_with_program(0x0500, &[0x4E71])?;
        machine.bus_mut().mem.chip_ram[0x60000] = 0xA5;

        // OCS (512 KiB reach, A19/A20 unused): the image repeats every
        // 512 KiB, and a write through an image lands in the RAM cell.
        assert_eq!(machine.debug_read_memory(0x0E0000, 1)[0], 0xA5);
        assert_eq!(machine.debug_read_memory(0x160000, 1)[0], 0xA5);
        assert_eq!(machine.debug_write_memory(0x1E0004, &[0x77]), 1);
        assert_eq!(machine.bus().mem.chip_ram[0x60004], 0x77);

        // 8372A (1 MiB reach, A20 unused): $0E0000 selects the unfitted
        // second DRAM bank (open bus) while $160000 is the A20 image of
        // $060000.
        machine
            .bus_mut()
            .set_agnus_revision(AgnusRevision::Ecs8372Rev4);
        assert_eq!(machine.debug_read_memory(0x0E0000, 1)[0], 0xFF);
        assert_eq!(machine.debug_write_memory(0x0E0000, &[0x11]), 0);
        assert_eq!(machine.debug_read_memory(0x160000, 1)[0], 0xA5);

        // 8375/Alice (2 MiB reach): the full window is decoded, so nothing
        // past the fitted RAM answers.
        machine.bus_mut().set_agnus_revision(AgnusRevision::Ecs8375);
        assert_eq!(machine.debug_read_memory(0x0E0000, 1)[0], 0xFF);
        assert_eq!(machine.debug_read_memory(0x160000, 1)[0], 0xFF);
        Ok(())
    }

    fn assert_single_instruction_timing(
        label: &str,
        slice: CpuStepSlice,
        expected_cpu_cck: u32,
        expected_bus_cck: u32,
    ) {
        assert_eq!(slice.instructions, 1, "{label}: retired instruction count");
        assert_eq!(
            slice.cpu_cycles.div_ceil(2).max(1),
            expected_cpu_cck,
            "{label}: m68k CPU cycles"
        );
        assert_eq!(slice.cpu_cck, expected_cpu_cck, "{label}: m68k CPU CCK");
        assert_eq!(
            slice.bus_advanced_cck, expected_bus_cck,
            "{label}: chip-bus wait CCK"
        );
    }

    #[test]
    fn reset_loads_sr_stack_and_pc() -> Result<()> {
        let bus = test_bus(reset_rom(0x0007_FFFE, 0x00F8_0100));
        let machine = M68kMachine::new(bus, CpuModel::M68000, false)?;
        assert_eq!(machine.sr(), 0x2700);
        assert_eq!(machine.a(7), 0x0007_FFFE);
        assert_eq!(machine.pc(), 0x00F8_0100);
        Ok(())
    }

    #[test]
    fn reset_overlay_reads_rom_without_preseeding_chip_ram() {
        let rom = reset_rom(0x1111_4EF9, 0x00FC_00D2);
        let mut bus = CpuBus {
            bus: Bus::new(
                Memory {
                    chip_ram: vec![0; 512 * 1024],
                    slow_ram: Vec::new(),
                    mb_ram: Vec::new(),
                    accel_ram: Vec::new(),
                    rom,
                    overlay: true,
                    zorro: ZorroChain::default(),
                    extended_rom: Vec::new(),
                    extended_rom_base: 0,
                    wcs: Vec::new(),
                    wcs_write_protected: false,
                },
                Paula::new(Box::new(StdoutSink::new()), Box::new(NullSink)),
                FloppyController::default(),
            ),
            address_mask: ADDRESS_MASK_24BIT,
            dbg_memw_addr: None,
            dbg_memw_hit: None,
            icache: None,
            dcache: None,
            dbg_irq_window: None,
            unmapped_read_continues_word: false,
            last_fetch_cache_hit: false,
            instruction_fetches_cached: false,
            sampled_irq_level: 0,
            ipl_sample_held: false,
            diag_current_pc: 0,
            jit_enabled: false,
        };

        assert_eq!(bus.read_long(0), 0x1111_4EF9);
        assert_eq!(&bus.bus.mem.chip_ram[0..8], &[0; 8]);

        bus.write_long(0, 0xDEAD_BEEF);
        assert_eq!(&bus.bus.mem.chip_ram[0..4], &[0; 4]);

        bus.bus.mem.overlay = false;
        bus.write_long(0, 0xDEAD_BEEF);
        assert_eq!(bus.read_long(0), 0xDEAD_BEEF);
    }

    #[test]
    fn chip_fetch_grants_bus_slots() {
        let mut bus = CpuBus {
            bus: test_bus(reset_rom(0, 0)),
            address_mask: ADDRESS_MASK_24BIT,
            dbg_memw_addr: None,
            dbg_memw_hit: None,
            icache: None,
            dcache: None,
            dbg_irq_window: None,
            unmapped_read_continues_word: false,
            last_fetch_cache_hit: false,
            instruction_fetches_cached: false,
            sampled_irq_level: 0,
            ipl_sample_held: false,
            diag_current_pc: 0,
            jit_enabled: false,
        };
        bus.bus.set_cpu_bus_arbitration_enabled(true);
        // Start on a refresh slot (0x003) so the fetch both waits for and then
        // consumes a chip bus slot: one missed color clock plus one granted.
        bus.bus.agnus.hpos = 0x003;
        let _ = bus.read_immediate_word(0);
        let (cck, _) = bus.bus.take_slice_bus_advance();
        assert!(cck >= 2);
    }

    #[test]
    fn three_byte_operand_costs_one_alice_slot_not_a_word_plus_a_byte() {
        // The 020/030 size an operand as a byte, word, three-byte or long
        // (MC68020UM 5.3.1), so a memory bit field spanning three bytes is a
        // single transfer. Alice moves anything up to a longword in one bus
        // slot, so on AGA that costs exactly what a long costs -- and strictly
        // less than the word-plus-byte pair the core composes when the bus
        // offers no three-byte transfer.
        //
        // Regression: timing-test/bfprobe.asm row 10 (`bfextu (a0){4:16}`)
        // measured the composed form billing the extra slot, reading 0.5% high
        // against a real A1200 while every other bit-field span matched.
        use crate::chipset::agnus::AgnusRevision;
        use crate::chipset::denise::DeniseRevision;

        let cost = |access: &mut dyn FnMut(&mut CpuBus)| -> u32 {
            let mut bus = CpuBus {
                bus: test_bus(reset_rom(0, 0)),
                address_mask: ADDRESS_MASK_24BIT,
                dbg_memw_addr: None,
                dbg_memw_hit: None,
                icache: None,
                dcache: None,
                dbg_irq_window: None,
                unmapped_read_continues_word: false,
                last_fetch_cache_hit: false,
                instruction_fetches_cached: false,
                sampled_irq_level: 0,
                ipl_sample_held: false,
                diag_current_pc: 0,
                jit_enabled: false,
            };
            bus.bus
                .set_chipset_revisions(AgnusRevision::AgaAlice, DeniseRevision::AgaLisa);
            bus.bus.set_cpu_bus_arbitration_enabled(true);
            // Same starting slot for every case, so the comparison is of slots
            // consumed rather than of where in the line the access began.
            bus.bus.agnus.hpos = 0x040;
            access(&mut bus);
            bus.bus.take_slice_bus_advance().0
        };

        // A longword-aligned three-byte field, entirely inside one longword.
        let three = cost(&mut |bus| {
            let _ = bus.read_three_bytes(CHIP_RAM_BASE as u32);
        });
        let long = cost(&mut |bus| {
            let _ = bus.read_long(CHIP_RAM_BASE as u32);
        });
        let composed = cost(&mut |bus| {
            let _ = bus.read_word(CHIP_RAM_BASE as u32);
            let _ = bus.read_byte(CHIP_RAM_BASE as u32 + 2);
        });

        assert_eq!(
            three, long,
            "a three-byte operand inside one longword must cost what a long costs on Alice"
        );
        assert!(
            three < composed,
            "three-byte transfer ({three} cck) must beat the composed word+byte ({composed} cck)"
        );
    }

    #[test]
    fn three_byte_operand_over_custom_registers_splits_into_word_and_byte() {
        // Dynamic bus sizing: the chipset answers a three-byte request at its
        // own 16-bit width, so the processor runs a word cycle and then a byte
        // cycle. That matters beyond timing here, because a device decodes a
        // register per access -- a single three-byte read of $DFFxxx would name
        // no register, so the second half has to be its own access or the third
        // byte never comes from INTREQR at all.
        let mut bus = CpuBus {
            bus: test_bus(reset_rom(0, 0)),
            address_mask: ADDRESS_MASK_24BIT,
            dbg_memw_addr: None,
            dbg_memw_hit: None,
            icache: None,
            dcache: None,
            dbg_irq_window: None,
            unmapped_read_continues_word: false,
            last_fetch_cache_hit: false,
            instruction_fetches_cached: false,
            sampled_irq_level: 0,
            ipl_sample_held: false,
            diag_current_pc: 0,
            jit_enabled: false,
        };
        // INTENA/INTREQ, whose read ports INTENAR ($01C) and INTREQR ($01E) are
        // adjacent and beam-independent, so the pair reads back deterministically.
        bus.bus.custom_write(0x09A, 2, 0x8000 | 0x4020);
        bus.bus.custom_write(0x09C, 2, 0x8000 | 0x2000);

        let base = CUSTOM_BASE + 0x01C;
        let composed =
            ((bus.read_word(base) as u32) << 8) | u32::from(bus.read_byte(base.wrapping_add(2)));
        let three = bus.read_three_bytes(base);

        assert_eq!(
            three, composed,
            "a three-byte span over a 16-bit port must read what its word and byte cycles read"
        );
        // A single sized access would reach custom_read as one 3-byte read,
        // which names no register and answers with just the word at `base`.
        assert_ne!(
            three,
            u32::from(bus.read_word(base)),
            "INTENAR alone is a dropped third byte, not a three-byte operand"
        );
    }

    #[test]
    fn three_byte_operand_moves_exactly_three_bytes() {
        // The transfer must not touch the byte past the field: on a
        // memory-mapped register or at the end of a mapped region that byte is
        // observable, not merely mistimed.
        let mut bus = CpuBus {
            bus: test_bus(reset_rom(0, 0)),
            address_mask: ADDRESS_MASK_24BIT,
            dbg_memw_addr: None,
            dbg_memw_hit: None,
            icache: None,
            dcache: None,
            dbg_irq_window: None,
            unmapped_read_continues_word: false,
            last_fetch_cache_hit: false,
            instruction_fetches_cached: false,
            sampled_irq_level: 0,
            ipl_sample_held: false,
            diag_current_pc: 0,
            jit_enabled: false,
        };
        let base = CHIP_RAM_BASE as u32;
        write_chip_long(&mut bus.bus, base, 0x1122_3344);
        assert_eq!(bus.read_three_bytes(base), 0x0011_2233);

        bus.write_three_bytes(base, 0x00AA_BBCC);
        assert_eq!(
            read_chip_long(&bus.bus, base),
            0xAABB_CC44,
            "the fourth byte must be untouched"
        );
    }

    #[test]
    fn slow_ram_contends_on_chip_bus_unlike_fast_ram() {
        // Trapdoor/"ranger" RAM at $C00000 is decoded by Gary and reached
        // through Agnus on the chip bus, so a CPU access that starts on an
        // occupied chip-bus slot (here a refresh slot) must wait for a free
        // slot -- exactly like chip RAM, and unlike uncontended fast RAM.
        //
        // Regression: when slow RAM was treated as external memory it ran at
        // full uncontended speed, so a slow-RAM-resident main loop executed too
        // fast and a per-frame interrupt could land in a one-instruction window
        // where the code had momentarily cleared DMACON.BLTEN, starting a blit
        // that could never run -> permanent hang.
        let mut bus = CpuBus {
            bus: test_bus(reset_rom(0, 0)),
            address_mask: ADDRESS_MASK_24BIT,
            dbg_memw_addr: None,
            dbg_memw_hit: None,
            icache: None,
            dcache: None,
            dbg_irq_window: None,
            unmapped_read_continues_word: false,
            last_fetch_cache_hit: false,
            instruction_fetches_cached: false,
            sampled_irq_level: 0,
            ipl_sample_held: false,
            diag_current_pc: 0,
            jit_enabled: false,
        };
        bus.bus.set_cpu_bus_arbitration_enabled(true);

        // Slow RAM: starting on a refresh slot the access stalls until a free
        // chip-bus slot, so it costs more than the bare two-color-clock cycle.
        bus.bus.agnus.hpos = 0x003;
        let _ = bus.read_word(SLOW_RAM_BASE as u32);
        let (slow_cck, _) = bus.bus.take_slice_bus_advance();
        assert!(
            slow_cck > 2,
            "slow RAM should stall on a busy chip-bus slot, got {slow_cck} cck"
        );

        // Fast RAM: off the chip bus, so the same access does not stall -- just
        // the 68000's two-color-clock bus cycle.
        bus.bus.agnus.hpos = 0x003;
        let _ = bus.read_word(FAST_RAM_BASE as u32);
        let (fast_cck, _) = bus.bus.take_slice_bus_advance();
        assert_eq!(fast_cck, 2, "fast RAM access is uncontended");
    }

    // Program fragment shared by the icache tests: enable the icache via
    // MOVEC, execute the NOP at $106 (caching it), patch $106 to
    // ADDQ.L #1,D2, then jump back to $106.
    //   $100: 7001            moveq #1,d0          (CACR EI)
    //   $102: 4E7B 0002       movec d0,cacr
    //   $106: 4E71            nop                  (patch target)
    //   $108: 31FC 5282 0106  move.w #$5282,($106).w   (addq.l #1,d2)
    const ICACHE_SMC_PROLOGUE: [u16; 7] = [0x7001, 0x4E7B, 0x0002, 0x4E71, 0x31FC, 0x5282, 0x0106];

    #[test]
    fn timing_cache_case_requires_every_instruction_fetch_to_hit() {
        let mut icache = crate::cache::CpuCache::default();
        icache.enabled = true;
        let mut bus = CpuBus {
            bus: test_bus(reset_rom(0, 0)),
            address_mask: ADDRESS_MASK_24BIT,
            dbg_memw_addr: None,
            dbg_memw_hit: None,
            icache: Some(Box::new(icache)),
            dcache: None,
            dbg_irq_window: None,
            unmapped_read_continues_word: false,
            last_fetch_cache_hit: false,
            instruction_fetches_cached: false,
            sampled_irq_level: 0,
            ipl_sample_held: false,
            diag_current_pc: 0,
            jit_enabled: false,
        };

        // Warm the opcode longword and a separate extension longword.
        let _ = bus.read_immediate_word(0x100);
        let _ = bus.read_immediate_word(0x108);

        bus.begin_instruction_fetches();
        let _ = bus.read_immediate_word(0x100);
        let _ = bus.read_immediate_word(0x108);
        assert!(
            bus.instruction_fetches_were_cached(),
            "all-resident instruction stream must select Cache Case"
        );

        bus.begin_instruction_fetches();
        let _ = bus.read_immediate_word(0x100);
        let _ = bus.read_immediate_word(0x110);
        assert!(
            !bus.instruction_fetches_were_cached(),
            "an extension-word miss must select Worst Case even after an opcode hit"
        );
    }

    #[test]
    fn icache_serves_stale_opcode_until_cacr_clear() -> Result<()> {
        // Authentic 68020 behaviour: the icache does not snoop writes, so
        // self-modifying code executes the stale cached opcode until the
        // cache is cleared through CACR.
        let mut program = ICACHE_SMC_PROLOGUE.to_vec();
        program.extend_from_slice(&[0x4EF8, 0x0106]); // $10E: jmp ($106).w
        let mut machine = M68kMachine::new(
            test_bus(reset_rom(0x0007_FFFE, 0x0000_0100)),
            CpuModel::M68EC020,
            false,
        )?;
        machine.set_cache_emulation(true, false);
        write_program(machine.bus_mut(), 0x100, &program);

        // moveq, movec, nop, move.w (patch), jmp, then the instruction at
        // $106 again: a cache hit, so still the stale NOP.
        let slice = machine.step_slice(6)?;
        assert_eq!(slice.instructions, 6);
        assert_eq!(
            machine.d(2),
            0,
            "stale cached NOP must execute, not the patch"
        );

        // Without the cache model the same sequence executes the patched
        // ADDQ on the revisit.
        let mut machine = M68kMachine::new(
            test_bus(reset_rom(0x0007_FFFE, 0x0000_0100)),
            CpuModel::M68EC020,
            false,
        )?;
        write_program(machine.bus_mut(), 0x100, &program);
        let _ = machine.step_slice(6)?;
        assert_eq!(machine.d(2), 1, "without icache the patched opcode runs");
        Ok(())
    }

    #[test]
    fn cacr_clear_strobe_flushes_icache_for_patched_code() -> Result<()> {
        let mut program = ICACHE_SMC_PROLOGUE.to_vec();
        program.extend_from_slice(&[
            0x7009, // $10E: moveq #9,d0   (CACR EI|CI: clear cache, keep enabled)
            0x4E7B, 0x0002, // $110: movec d0,cacr
            0x4EF8, 0x0106, // $114: jmp ($106).w
        ]);
        let mut machine = M68kMachine::new(
            test_bus(reset_rom(0x0007_FFFE, 0x0000_0100)),
            CpuModel::M68EC020,
            false,
        )?;
        machine.set_cache_emulation(true, false);
        write_program(machine.bus_mut(), 0x100, &program);

        // moveq, movec, nop, patch, moveq, movec (CI), jmp, then the
        // patched ADDQ really executes because the cache was cleared.
        let slice = machine.step_slice(8)?;
        assert_eq!(slice.instructions, 8);
        assert_eq!(machine.d(2), 1, "CACR CI must flush the stale entry");
        Ok(())
    }

    #[test]
    fn icache_68040_enabled_by_ie_and_flushed_by_cinv() -> Result<()> {
        // The 68040 CACR enables the instruction cache through IE (bit 15),
        // and CINV/CPUSH - not a CACR strobe - invalidates it. Self-modifying
        // code therefore runs the stale cached opcode until a CINV.
        //
        //   $100: 203C 0000 8000  move.l #$8000,d0        (CACR.IE)
        //   $106: 4E7B 0002        movec d0,cacr           (icache on)
        //   $10A: 4E71             nop                     (cached on first run)
        //   $10C: 31FC 5282 010A   move.w #$5282,($10A).w  (patch -> addq.l #1,d2)
        //   $112: F498             cinva (instruction)     (clear icache)  [B only]
        //   ...:  4EF8 010A        jmp ($10A).w
        let prologue: [u16; 9] = [
            0x203C, 0x0000, 0x8000, 0x4E7B, 0x0002, 0x4E71, 0x31FC, 0x5282, 0x010A,
        ];

        // A: no CINV. The patched write does not snoop the icache, so the
        // revisit hits the stale NOP and d2 stays 0.
        let mut program_a = prologue.to_vec();
        program_a.extend_from_slice(&[0x4EF8, 0x010A]); // jmp ($10A).w
        let mut machine = M68kMachine::new(
            test_bus(reset_rom(0x0007_FFFE, 0x0000_0100)),
            CpuModel::M68040,
            false,
        )?;
        machine.set_cache_emulation(true, false);
        write_program(machine.bus_mut(), 0x100, &program_a);
        let slice = machine.step_slice(6)?;
        assert_eq!(slice.instructions, 6);
        assert_eq!(machine.d(2), 0, "stale cached NOP must run without a CINV");

        // B: CINVA before the jump flushes the icache, so the revisit misses
        // and refetches the patched ADDQ.
        let mut program_b = prologue.to_vec();
        program_b.extend_from_slice(&[0xF498, 0x4EF8, 0x010A]); // cinva (instr); jmp
        let mut machine = M68kMachine::new(
            test_bus(reset_rom(0x0007_FFFE, 0x0000_0100)),
            CpuModel::M68040,
            false,
        )?;
        machine.set_cache_emulation(true, false);
        write_program(machine.bus_mut(), 0x100, &program_b);
        let slice = machine.step_slice(7)?;
        assert_eq!(slice.instructions, 7);
        assert_eq!(machine.d(2), 1, "CINVA must flush the stale icache entry");
        Ok(())
    }

    #[test]
    fn cpu_040_caches_are_4kb() {
        // The 040 installs the larger 4 KB (1024-longword) caches; the 020/030
        // get the 256-byte (64) ones.
        assert_eq!(
            cache_lines_for_cpu_type(CpuType::M68LC040),
            crate::cache::LINES_040
        );
        assert_eq!(
            cache_lines_for_cpu_type(CpuType::M68020),
            crate::cache::LINES_020
        );
    }

    #[test]
    fn icache_removes_chip_bus_fetch_traffic_in_loops() -> Result<()> {
        // moveq #1,d0 / movec d0,cacr / moveq #10,d1 /
        // loop: nop / dbra d1,loop  -- all in chip RAM.
        let program: [u16; 8] = [
            0x7001, 0x4E7B, 0x0002, 0x720A, 0x4E71, 0x51C9, 0xFFFC, 0x4E71,
        ];
        let steps = 3 + 2 * 11;
        let run = |icache: bool| -> Result<u32> {
            // Run at the 68EC020's native ~14 MHz (4 CPU clocks per cck): with
            // the accurate 3-clock 020 chip-bus cycle a word fetch is one cck,
            // which still bus-bounds the tiny in-loop instructions, so dropping
            // those fetches to the cache cuts bus traffic. (At the stock 2-clock
            // ratio the 1.5-cck fetch already fits inside the instruction time,
            // so cached and uncached totals match and there is nothing to cut.)
            let mut machine = build(
                test_bus(reset_rom(0x0007_FFFE, 0x0000_0100)),
                CpuModel::M68EC020,
                false,
                4,
                Default::default(),
                false,
            )?;
            machine.set_cache_emulation(icache, false);
            write_program(machine.bus_mut(), 0x100, &program);
            let mut bus_cck = 0u32;
            let mut instructions = 0usize;
            while instructions < steps {
                let slice = machine.step_slice(steps - instructions)?;
                assert!(slice.instructions > 0);
                instructions += slice.instructions;
                bus_cck += slice.bus_advanced_cck;
            }
            Ok(bus_cck)
        };
        let without = run(false)?;
        let with = run(true)?;
        assert!(
            with < without,
            "icache hits must stop competing for the chip bus: {with} >= {without} cck"
        );
        Ok(())
    }

    #[test]
    fn dcache_caches_expansion_ram_but_not_chip_ram() {
        let mut dcache = crate::cache::CpuCache::default();
        dcache.enabled = true;
        let mut bus = CpuBus {
            bus: test_bus(reset_rom(0, 0)),
            address_mask: ADDRESS_MASK_24BIT,
            dbg_memw_addr: None,
            dbg_memw_hit: None,
            icache: None,
            dcache: Some(Box::new(dcache)),
            dbg_irq_window: None,
            unmapped_read_continues_word: false,
            last_fetch_cache_hit: false,
            instruction_fetches_cached: false,
            sampled_irq_level: 0,
            ipl_sample_held: false,
            diag_current_pc: 0,
            jit_enabled: false,
        };
        bus.bus.set_cpu_bus_arbitration_enabled(true);
        let fast = FAST_RAM_BASE as u32 + 0x40;
        bus.write_long(fast, 0xCAFE_F00D);
        let _ = bus.bus.take_slice_bus_advance();

        // First read misses and is billed; the refill peeks memory.
        assert_eq!(bus.read_long(fast), 0xCAFE_F00D);
        let (miss_cck, _) = bus.bus.take_slice_bus_advance();
        assert!(miss_cck > 0);

        // Second read is a hit: no bus time at all.
        assert_eq!(bus.read_long(fast), 0xCAFE_F00D);
        let (hit_cck, _) = bus.bus.take_slice_bus_advance();
        assert_eq!(hit_cck, 0, "dcache hit must cost no bus cycles");

        // A write invalidates (write-through), so the next read re-bills
        // and sees the new value.
        bus.write_long(fast, 0x0BAD_C0DE);
        let _ = bus.bus.take_slice_bus_advance();
        assert_eq!(bus.read_long(fast), 0x0BAD_C0DE);
        let (refill_cck, _) = bus.bus.take_slice_bus_advance();
        assert!(refill_cck > 0);

        // Chip RAM is cache-inhibited for data: two reads, two bills.
        bus.write_long(0x3000, 0x1234_5678);
        let _ = bus.bus.take_slice_bus_advance();
        assert_eq!(bus.read_long(0x3000), 0x1234_5678);
        let (first, _) = bus.bus.take_slice_bus_advance();
        assert_eq!(bus.read_long(0x3000), 0x1234_5678);
        let (second, _) = bus.bus.take_slice_bus_advance();
        assert!(first > 0 && second > 0, "chip RAM data reads stay uncached");
    }

    #[test]
    fn device_tick_dma_invalidates_dcache_at_coherent_flush() {
        // The copperhf board completes reads asynchronously: the payload is
        // DMA'd into the caller's buffer by the board's tick(), not by the
        // doorbell access itself, so the synchronous device-access path's
        // dcache drop never sees it. This pins the tick-path counterpart:
        // the drain sets Bus::devices_wrote_memory, and
        // flush_timed_devices_dcache_coherent drops the modelled data cache
        // so the DMA'd bytes read back fresh (the 68040+PFS3 corruption).
        let mut dcache = crate::cache::CpuCache::default();
        dcache.enabled = true;
        let mut bus = CpuBus {
            bus: test_bus(reset_rom(0, 0)),
            address_mask: ADDRESS_MASK_24BIT,
            dbg_memw_addr: None,
            dbg_memw_hit: None,
            icache: None,
            dcache: Some(Box::new(dcache)),
            dbg_irq_window: None,
            unmapped_read_continues_word: false,
            last_fetch_cache_hit: false,
            instruction_fetches_cached: false,
            sampled_irq_level: 0,
            ipl_sample_held: false,
            diag_current_pc: 0,
            jit_enabled: false,
        };
        bus.bus.set_cpu_bus_arbitration_enabled(true);

        // A tiny flat disk image (no DOS/RDSK signature, so every LBA is a
        // direct file offset), filled with the i % 251 pattern.
        let path =
            std::env::temp_dir().join(format!("copperline-cpu-dcache-dma-{}", std::process::id()));
        let bytes: Vec<u8> = (0..16 * 512).map(|i| (i % 251) as u8).collect();
        std::io::Write::write_all(&mut std::fs::File::create(&path).unwrap(), &bytes).unwrap();
        let image = crate::harddrive::HardDriveImage::open(
            &path,
            "CHF0",
            "copperhf",
            None,
            0,
            crate::diskimage::FileSystem::OFS,
        )
        .unwrap();
        let mut board = crate::copperhf::CopperhfBoard::new();
        board.attach_unit(0, image);
        bus.bus
            .devices
            .push(crate::zorro_device::BoardDevice::Copperhf(board));

        // CMD_READ of sector 0 into a fast-RAM buffer (dcache-cacheable).
        // The IOStdReq itself lives in chip RAM like the copperhf tests'.
        let req = 0x1000u32;
        let buf = FAST_RAM_BASE as u32 + 0x400;
        let hdr = &mut bus.bus.mem.chip_ram;
        hdr[req as usize + 24..][..4].copy_from_slice(&0u32.to_be_bytes()); // io_Unit
        hdr[req as usize + 28..][..2].copy_from_slice(&2u16.to_be_bytes()); // CMD_READ
        hdr[req as usize + 36..][..4].copy_from_slice(&512u32.to_be_bytes()); // io_Length
        hdr[req as usize + 40..][..4].copy_from_slice(&buf.to_be_bytes()); // io_Data
        hdr[req as usize + 44..][..4].copy_from_slice(&0u32.to_be_bytes()); // io_Offset
        {
            let mut host = crate::zorro_device::DeviceHost::for_slot(&mut bus.bus.mem, 0);
            crate::zorro_device::ZorroDevice::write(
                &mut bus.bus.devices[0],
                0x4020, // CHF_DOORBELL
                4,
                req,
                &mut host,
            );
        }

        // Prime the data cache over the buffer while the read is in flight:
        // the buffer still holds zeros, and the fill makes the line stale
        // the moment the drain lands.
        assert_eq!(bus.read_long(buf), 0);

        // Tick until the drain DMAs the sector in (peek bypasses the cache).
        let expected = u32::from_be_bytes([0, 1, 2, 3]);
        let mut drained = false;
        for _ in 0..2000 {
            bus.bus.advance_devices(16);
            if bus.peek_long(buf) == expected {
                drained = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        assert!(drained, "copperhf never drained the completed read");

        // Instruction-boundary path: the coherent flush consumes the latch
        // and drops the cache, so the boundary's first read is fresh --
        // exactly what a 68040 guest waiting on the INT2 reply needs (the
        // corruption this fixes was PFS3 reading stale file data here).
        bus.flush_timed_devices_dcache_coherent();
        assert_eq!(
            bus.read_long(buf),
            expected,
            "coherent flush must drop the dcache so the DMA'd data is seen"
        );

        // Read-hit path: a second read (sector 1) drains while the buffer's
        // line sits freshly cached, and NO boundary flush runs -- the read
        // itself must consume the latch (the JIT batch path can flush timed
        // devices mid-batch, so a polling driver reads without ever passing
        // an instruction boundary).
        let hdr = &mut bus.bus.mem.chip_ram;
        hdr[req as usize + 44..][..4].copy_from_slice(&512u32.to_be_bytes()); // io_Offset
        {
            let mut host = crate::zorro_device::DeviceHost::for_slot(&mut bus.bus.mem, 0);
            crate::zorro_device::ZorroDevice::write(
                &mut bus.bus.devices[0],
                0x4020, // CHF_DOORBELL
                4,
                req,
                &mut host,
            );
        }
        let expected2 = u32::from_be_bytes([10, 11, 12, 13]); // 512..515 % 251
        let mut drained = false;
        for _ in 0..2000 {
            bus.bus.advance_devices(16);
            if bus.peek_long(buf) == expected2 {
                drained = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        assert!(drained, "copperhf never drained the second read");
        assert_eq!(
            bus.read_long(buf),
            expected2,
            "a data read must consume the DMA latch itself (JIT mid-batch case)"
        );

        // Precision: idle ticks (no DMA in flight) must not latch a write --
        // a board merely borrowing memory on its tick would otherwise clear
        // the data cache every tick and disable it in practice.
        bus.bus.advance_devices(64);
        assert!(
            !bus.bus.take_devices_wrote_memory(),
            "idle device ticks must not latch a DMA write"
        );
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn rom_writes_are_ignored() {
        let mut bus = CpuBus {
            bus: test_bus(reset_rom(0, 0)),
            address_mask: ADDRESS_MASK_24BIT,
            dbg_memw_addr: None,
            dbg_memw_hit: None,
            icache: None,
            dcache: None,
            dbg_irq_window: None,
            unmapped_read_continues_word: false,
            last_fetch_cache_hit: false,
            instruction_fetches_cached: false,
            sampled_irq_level: 0,
            ipl_sample_held: false,
            diag_current_pc: 0,
            jit_enabled: false,
        };
        let before = bus.read_long(ROM_BASE as u32);
        bus.write_long(ROM_BASE as u32, 0xDEAD_BEEF);
        assert_eq!(bus.read_long(ROM_BASE as u32), before);
    }

    #[test]
    fn unmapped_cpu_reads_float_to_chip_data_bus() {
        // An unmapped CPU read does not return a fixed all-ones pattern; it
        // floats to the last value the Agnus-arbitrated chip data bus carried
        // (display/audio DMA), exactly as on real hardware. Returning a constant
        // can close pathological pointer-chase loops (a program walking a
        // corrupted list off into unmapped space) that terminate on real silicon
        // because the floating value keeps changing.
        let mut bus = CpuBus {
            bus: test_bus(reset_rom(0, 0)),
            address_mask: ADDRESS_MASK_24BIT,
            dbg_memw_addr: None,
            dbg_memw_hit: None,
            icache: None,
            dcache: None,
            dbg_irq_window: None,
            unmapped_read_continues_word: false,
            last_fetch_cache_hit: false,
            instruction_fetches_cached: false,
            sampled_irq_level: 0,
            ipl_sample_held: false,
            diag_current_pc: 0,
            jit_enabled: false,
        };
        // $F00000 with no extended ROM fitted is genuinely undecoded (the
        // chip-register space at $C00000-$DFFFFF is not: Gary mirrors the
        // custom registers there, see custom_reg_page_offset).
        let addr = 0x00F0_0000;
        // A write into unmapped space still drives the bus: the following
        // undriven read floats to the last word of the transfer.
        bus.write_long(addr, 0x1234_5678);
        assert_eq!(bus.read_long(addr), 0x5678_5678);
        // Any latched value is what undriven reads float to; a 68000 byte
        // read takes the high data-bus byte at even addresses and the low
        // byte at odd.
        bus.bus.data_bus = 0xA53C;
        assert_eq!(bus.read_byte(addr), 0xA5);
        assert_eq!(bus.read_byte(addr + 1), 0x3C);
        assert_eq!(bus.read_word(addr), 0xA53C);
        assert_eq!(bus.read_long(addr), 0xA53C_A53C);
    }

    fn cpu_bus(bus: Bus) -> CpuBus {
        CpuBus {
            bus,
            address_mask: ADDRESS_MASK_24BIT,
            dbg_memw_addr: None,
            dbg_memw_hit: None,
            icache: None,
            dcache: None,
            dbg_irq_window: None,
            unmapped_read_continues_word: false,
            last_fetch_cache_hit: false,
            instruction_fetches_cached: false,
            sampled_irq_level: 0,
            ipl_sample_held: false,
            diag_current_pc: 0,
            jit_enabled: false,
        }
    }

    /// `getreg`/`putreg` as every AmigaOS clock probe writes them: the
    /// four-bit part answers at `base + reg * 4 + 3` and the read is masked
    /// to the nibble.
    fn clock_reg(bus: &mut CpuBus, reg: u32) -> u8 {
        bus.read_byte(RTC_BASE + reg * 4 + 3) & 0x0F
    }

    fn set_clock_reg(bus: &mut CpuBus, reg: u32, value: u8) {
        bus.write_byte(RTC_BASE + reg * 4 + 3, value);
    }

    /// An empty clock socket must not be talked into existing. Every OS probe
    /// asks the same question -- write a control nibble, read it back -- so a
    /// lane that floated to the last value on the data bus would eventually
    /// echo the write and hand the guest a clock, and a date, that the machine
    /// does not have (issue #235: KS1.3 `SetClock` printing `<unset>` instead
    /// of "Battery Backed up Clock not found", AROS inventing a past date).
    #[test]
    fn an_empty_clock_socket_never_reads_back_a_written_control_nibble() {
        let mut bus = cpu_bus(test_bus(reset_rom(0, 0)));
        bus.bus.set_rtc_present(false);

        // Whatever the bus last carried -- including the value about to be
        // written, which is the case that invented a clock.
        for float in [0x0000u16, 0x0404, 0x4444, 0xFFFF, 0xA53C] {
            for reg in 0x0u32..=0xF {
                for nibble in 0x0u8..=0xF {
                    bus.bus.data_bus = float;
                    set_clock_reg(&mut bus, reg, nibble);
                    bus.bus.data_bus = float;
                    assert_eq!(
                        clock_reg(&mut bus, reg),
                        0,
                        "reg {reg:#X} echoed {nibble:#X} with the bus at {float:#06X}"
                    );
                }
            }
        }
    }

    /// AROS `battclock.resource` init (arch/m68k-amiga), the probe from the
    /// issue, played out in full: it looks for an RF5C01A (control D reading
    /// 8 with control F reading 0) and an MSM6242B (control F reading 4),
    /// resetting the part and retrying once before giving up. With no clock
    /// fitted it must reach the end having found neither.
    #[test]
    fn the_aros_clock_probe_finds_nothing_in_an_empty_socket() {
        let mut bus = cpu_bus(test_bus(reset_rom(0, 0)));
        bus.bus.set_rtc_present(false);
        bus.bus.data_bus = 0x0404; // the float that used to fake an MSM6242B

        // "10 minutes or hours bit 3 set = not a clock" -- the early out.
        assert_eq!(clock_reg(&mut bus, 0x01) & 8, 0);
        assert_eq!(clock_reg(&mut bus, 0x03) & 8, 0);

        let mut found = false;
        for _ in 0..2 {
            if clock_reg(&mut bus, 0x0D) == 0x8 && clock_reg(&mut bus, 0x0F) == 0x0 {
                found = true; // RF5C01A
                break;
            }
            if clock_reg(&mut bus, 0x0F) == 0x4 {
                found = true; // MSM6242B
                break;
            }
            set_clock_reg(&mut bus, 0xE, 0);
            set_clock_reg(&mut bus, 0xD, 0);
            set_clock_reg(&mut bus, 0xD, 4); // set the IRQ flag...
            if clock_reg(&mut bus, 0xD) == 0 {
                // ...which an MSM6242B cannot hold, so reset it as one.
                set_clock_reg(&mut bus, 0xF, 7);
                set_clock_reg(&mut bus, 0xF, 4);
            } else {
                set_clock_reg(&mut bus, 0xF, 3);
                set_clock_reg(&mut bus, 0xF, 0);
                set_clock_reg(&mut bus, 0xD, 8);
            }
        }
        assert!(!found, "AROS found a clock in an empty socket");
    }

    /// The same probe against a fitted MSM6242 has to succeed, or the fix
    /// above would have cost the machine its clock: control F reads the
    /// 24-hour bit, which is the tell AROS matches on.
    #[test]
    fn the_aros_clock_probe_identifies_a_fitted_msm6242() {
        let mut bus = cpu_bus(test_bus(reset_rom(0, 0)));
        bus.bus.set_rtc_present(true);
        bus.bus.data_bus = 0xFFFF;

        assert_eq!(clock_reg(&mut bus, 0x01) & 8, 0);
        assert_eq!(clock_reg(&mut bus, 0x03) & 8, 0);
        assert_ne!(clock_reg(&mut bus, 0x0D), 0x8); // not an RF5C01A
        assert_eq!(clock_reg(&mut bus, 0x0F), 0x4); // an MSM6242B
    }

    /// The Ricoh branch of the same probe: an RP5C01 (the A3000/A4000
    /// part) answers with its power-on MODE, $8 (timer running, block 0),
    /// while the write-only RESET select reads zero -- exactly the pair
    /// AROS matches as an "RF5C01A" before ever considering an MSM6242.
    #[test]
    fn the_aros_clock_probe_identifies_a_fitted_rp5c01() {
        let mut bus = cpu_bus(test_bus(reset_rom(0, 0)));
        bus.bus.set_rtc_chip(crate::rtc::RtcChip::Rp5c01);
        bus.bus.set_rtc_present(true);
        bus.bus.data_bus = 0xFFFF;

        assert_eq!(clock_reg(&mut bus, 0x01) & 8, 0);
        assert_eq!(clock_reg(&mut bus, 0x03) & 8, 0);
        assert_eq!(clock_reg(&mut bus, 0x0D), 0x8); // an RF5C01A...
        assert_eq!(clock_reg(&mut bus, 0x0F), 0x0); // ...whose RESET reads 0
    }

    /// The register select is A2-A5, so A1 never reaches the decode: each
    /// register answers at both of its odd bytes, `+1` as well as the `+3`
    /// AmigaOS uses by convention. Writes take the same lanes, so a value
    /// poked at `+1` is readable at `+3` and vice versa.
    #[test]
    fn a1_is_not_decoded_so_both_odd_bytes_reach_the_same_register() {
        let mut bus = cpu_bus(test_bus(reset_rom(0, 0)));
        bus.bus.set_rtc_present(true);

        // Control E (register $E) is plain readable/writable storage.
        let plus_one = RTC_BASE + 0xE * 4 + 1;
        let plus_three = RTC_BASE + 0xE * 4 + 3;
        bus.write_byte(plus_three, 0x5);
        assert_eq!(bus.read_byte(plus_one) & 0x0F, 0x5);
        bus.write_byte(plus_one, 0xA);
        assert_eq!(bus.read_byte(plus_three) & 0x0F, 0xA);
    }

    /// The clock sits on the low byte lane only: the even lane is not wired
    /// to it and floats whether or not a chip is in the socket. AROS reads a
    /// *word* at $DC0010 to test whether the custom registers are mirrored
    /// into clock space, so the even lane has to keep floating. A byte write
    /// aimed at that lane reaches nothing.
    #[test]
    fn only_the_odd_byte_lane_reaches_the_clock() {
        let mut bus = cpu_bus(test_bus(reset_rom(0, 0)));
        bus.bus.set_rtc_present(false);
        bus.bus.data_bus = 0xA53C;

        assert_eq!(bus.read_byte(RTC_BASE + 0x34), 0xA5);
        assert_eq!(
            bus.read_byte(RTC_BASE + 0x37),
            crate::rtc::EMPTY_SOCKET_LANE
        );
        assert_eq!(
            bus.read_word(RTC_BASE + 0x36),
            0xA500 | u16::from(crate::rtc::EMPTY_SOCKET_LANE)
        );

        // A byte write to the even lane goes nowhere, so control E keeps the
        // value the odd lane put there.
        bus.bus.set_rtc_present(true);
        bus.write_byte(RTC_BASE + 0xE * 4 + 3, 0x6);
        bus.write_byte(RTC_BASE + 0xE * 4, 0x9);
        assert_eq!(bus.read_byte(RTC_BASE + 0xE * 4 + 3) & 0x0F, 0x6);
    }

    #[test]
    fn fast_ram_window_ends_without_aliasing() {
        let mut bus = cpu_bus(test_bus(reset_rom(0, 0)));
        let first = FAST_RAM_BASE as u32;
        let last = first + 64 * 1024 - 2;
        let outside = first + 64 * 1024;

        bus.write_word(first, 0x1111);
        bus.write_word(last, 0x2222);
        assert_eq!(bus.read_word(first), 0x1111);
        assert_eq!(bus.read_word(last), 0x2222);

        // The first word beyond this configured Zorro RAM board is
        // undecoded. In particular, it must not wrap onto either edge.
        bus.write_word(outside, 0x3333);
        assert_eq!(bus.read_word(first), 0x1111);
        assert_eq!(bus.read_word(last), 0x2222);
    }

    #[test]
    fn a500_rev6_512k_chip_and_trapdoor_layout_is_cpu_visible() {
        use crate::chipset::agnus::AgnusRevision;

        let mut machine_bus = test_bus(reset_rom(0, 0));
        machine_bus.mem.zorro = ZorroChain::default();
        machine_bus.mem.slow_ram.resize(512 * 1024, 0);
        machine_bus.set_agnus_revision(AgnusRevision::Ecs8372Rev4);
        let mut bus = cpu_bus(machine_bus);

        // A Rev.6 board with an ECS 8372A still has only the fitted 512 KiB
        // motherboard bank at $000000-$07FFFF. The unfitted upper half of the
        // 1 MiB Agnus image is open, while A20 repeats the image at $100000.
        bus.write_word(0x0000_0000, 0x1111);
        bus.write_word(0x0007_FFFE, 0x2222);
        bus.write_word(0x0008_0000, 0x3333);
        assert_eq!(bus.read_word(0x0000_0000), 0x1111);
        assert_eq!(bus.read_word(0x0007_FFFE), 0x2222);
        bus.write_word(0x0010_0000, 0x4444);
        assert_eq!(bus.read_word(0x0000_0000), 0x4444, "A20 chip image");

        // The 512 KiB trapdoor expansion is separate slow RAM at
        // $C00000-$C7FFFF. The following address is the small-box custom-chip
        // mirror, not another image of the trapdoor RAM.
        bus.write_word(0x00C0_0000, 0x5555);
        bus.write_word(0x00C7_FFFE, 0x6666);
        bus.write_word(0x00C8_0000, 0x7777);
        assert_eq!(bus.read_word(0x00C0_0000), 0x5555);
        assert_eq!(bus.read_word(0x00C7_FFFE), 0x6666);
    }

    #[test]
    fn gary_mirrors_custom_registers_in_empty_ranger_space() {
        // On the small-box machines Gary's coarse decode answers custom
        // register accesses throughout the chip-register space wherever no
        // RAM claims the address. Kickstart's slow-RAM sizing relies on
        // this: at each 256K step it writes INTENA at $xxF09A and reads
        // INTENAR at $xxF01C, and treats the block as RAM until the mirror
        // answers with the bits it just cleared (KS 1.2 has no further
        // check, so without the mirror it sizes RAM into unmapped space
        // and dies with a yellow screen).
        let mut bus = cpu_bus(test_bus(reset_rom(0, 0)));

        // Set an INTENA bit through the canonical page, then run the KS
        // probe shape through the first mirror above the fixture's 64K of
        // slow RAM: the write must land in INTENA and the read must see it.
        bus.write_word(0x00DF_F09A, 0x8004);
        assert_eq!(bus.read_word(0x00CB_F01C), 0x0004, "mirror INTENAR read");
        bus.write_word(0x00CB_F09A, 0x3FFF);
        assert_eq!(bus.read_word(0x00DF_F01C), 0x0000, "mirror INTENA write");

        // The reserved $D8 pages and the $DE/$DF space below the canonical
        // page mirror the same registers.
        bus.write_word(0x00DB_F09A, 0x8004);
        assert_eq!(bus.read_word(0x00DE_F01C), 0x0004);
        bus.write_word(0x00DF_E09A, 0x3FFF);
        assert_eq!(bus.read_word(0x00DF_F01C), 0x0000);

        // Addresses backed by slow RAM stay RAM: the same probe offsets
        // inside the fixture's 64K round-trip as memory and leave INTENA
        // alone.
        bus.write_word(0x00C0_F09A, 0x8004);
        assert_eq!(bus.read_word(0x00C0_F09A), 0x8004);
        assert_eq!(bus.read_word(0x00DF_F01C), 0x0000);
    }

    #[test]
    fn gayle_and_fat_gary_machines_do_not_mirror_custom_registers() {
        // Gayle (A600/A1200) and Fat Gary/Ramsey (A3000/A4000) decode the
        // chip-register space properly, so the ranger space floats instead
        // of mirroring the custom registers.
        let mut gayle_bus = test_bus(reset_rom(0, 0));
        gayle_bus.attach_gayle(crate::gayle::Gayle::new(0xD0));
        let mut fat_gary_bus = test_bus(reset_rom(0, 0));
        fat_gary_bus.attach_gary(crate::gary::Gary::new());
        for machine_bus in [gayle_bus, fat_gary_bus] {
            let mut bus = cpu_bus(machine_bus);
            bus.write_word(0x00DF_F09A, 0x8004);
            bus.bus.data_bus = 0xA53C;
            assert_eq!(bus.read_word(0x00CB_F01C), 0xA53C, "floating bus");
            bus.write_word(0x00CB_F09A, 0x3FFF);
            assert_eq!(bus.read_word(0x00DF_F01C), 0x0004, "INTENA untouched");
        }
    }

    #[test]
    fn fpu_family_is_forced_to_line_f_when_fpu_disabled() -> Result<()> {
        let mut rom = reset_rom(0x0007_FFFE, 0x0000_0100);
        rom[0x2C..0x30].copy_from_slice(&0x0000_0200u32.to_be_bytes());
        rom[0x100..0x102].copy_from_slice(&0xF200u16.to_be_bytes());
        rom[0x200..0x202].copy_from_slice(&0x4E71u16.to_be_bytes());
        let mut machine = M68kMachine::new(test_bus(rom), CpuModel::M68EC020, false)?;
        let slice = machine.step_slice(1)?;
        assert_eq!(slice.instructions, 1);
        assert_eq!(machine.pc(), 0x0000_0200);
        assert_eq!(
            machine.bus.bus.take_cpu_bus_overlap_clocks(),
            0,
            "exception stack-write overlap is charged at the exception boundary"
        );
        Ok(())
    }

    #[test]
    fn cpu_bus_timing_charges_chip_ram_opcode_fetches() -> Result<()> {
        // With the MC68000 prefetch queue (m68k Part E.1), the first
        // instruction after a cold start costs its direct opcode fetch plus
        // the two-word queue top-up: 3 word accesses. Every access now advances
        // the bus by 2 cck (the 68000's 4-clock bus cycle), whether it lands on
        // the contended chip bus or an external region (fast/slow RAM, ROM), so
        // each region costs 3 accesses x 2 cck = 6 bus cck. (Steady state, a
        // one-word instruction costs 1 access = 2 cck: its final prefetch.)
        for (label, pc) in [
            ("chip RAM opcode fetch", 0x0000_0100),
            ("fast RAM opcode fetch", FAST_RAM_BASE as u32 + 0x0100),
            ("slow RAM opcode fetch", SLOW_RAM_BASE as u32 + 0x0100),
            ("ROM opcode fetch", ROM_BASE as u32 + 0x0100),
        ] {
            let slice = run_one_instruction_at(pc, &[0x4E71])?;
            assert_single_instruction_timing(label, slice, 2, 6);
        }
        Ok(())
    }

    #[test]
    fn cpu_bus_timing_charges_chip_ram_extension_words() -> Result<()> {
        // Cold-start prefetch accounting (see opcode-fetch test): direct
        // opcode fetch + direct/queued extension consumes + their prefetches
        // + the end-of-instruction top-up. Each word access advances the bus by
        // 2 cck. MOVE.W #imm,D0 makes 4 word accesses = 8 bus cck; MOVE.L
        // #imm,D0 makes 5 word accesses = 10 bus cck.
        assert_single_instruction_timing(
            "chip RAM word immediate extension",
            run_one_instruction_at(0x0000_0100, &[0x303C, 0x1234])?,
            4,
            8,
        );
        assert_single_instruction_timing(
            "chip RAM long immediate extension",
            run_one_instruction_at(0x0000_0100, &[0x203C, 0x1234, 0x5678])?,
            6,
            10,
        );
        Ok(())
    }

    #[test]
    fn cpu_bus_timing_charges_chip_ram_data_reads_and_writes() -> Result<()> {
        // run_rom_instruction runs the opcode from ROM; the data operand targets
        // chip RAM at $00000200. bus_advanced_cck is now the TOTAL device time of
        // the slice = every word access (ROM prefetches plus the chip data
        // access) at 2 cck each, plus any contention waits (none here at the
        // free hpos 0x21) and internal CPU clocks folded in via sync().
        assert_single_instruction_timing(
            "chip RAM word data read",
            run_rom_instruction(&[0x3039, 0x0000, 0x0200])?,
            8,
            12,
        );
        assert_single_instruction_timing(
            "chip RAM long data read",
            run_rom_instruction(&[0x2039, 0x0000, 0x0200])?,
            10,
            14,
        );
        assert_single_instruction_timing(
            "chip RAM word data write",
            run_rom_instruction(&[0x33FC, 0x1234, 0x0000, 0x0200])?,
            10,
            14,
        );
        assert_single_instruction_timing(
            "chip RAM long data write",
            run_rom_instruction(&[0x23FC, 0x1234, 0x5678, 0x0000, 0x0200])?,
            14,
            18,
        );
        Ok(())
    }

    #[test]
    fn cpu_data_reads_fire_read_but_not_write_watchpoints() -> Result<()> {
        let program = &[0x3039, 0x0000, 0x0400]; // MOVE.W $00000400,D0
        let pc = ROM_BASE as u32 + 0x0100;

        let mut read = machine_with_program(pc, program)?;
        read.ui_toggle_watch_access(0x0400, None, None, crate::debugger::WatchAccess::Read);
        read.step_slice(1)?;
        assert!(matches!(
            read.take_ui_debug_stop(),
            Some(crate::debugger::DebugStop::Watch {
                addr: 0x0400,
                access: crate::debugger::WatchAccess::Read,
                source: crate::debugger::WatchSource::Cpu,
                ..
            })
        ));

        let mut write = machine_with_program(pc, program)?;
        write.ui_toggle_watch(0x0400);
        write.step_slice(1)?;
        assert!(write.take_ui_debug_stop().is_none());
        Ok(())
    }

    #[test]
    fn access_watch_rebaselines_a_read_modify_write_before_resume() -> Result<()> {
        let pc = ROM_BASE as u32 + 0x0100;
        let program = &[
            0x4AF9, 0x0000, 0x0400, // TAS.B $00000400
            0x4E71, // NOP
        ];
        let mut machine = machine_with_program(pc, program)?;
        machine.bus_mut().mem.chip_ram[0x0400] = 0x12;
        machine.ui_toggle_watch_access(0x0400, None, None, crate::debugger::WatchAccess::Access);

        machine.step_slice(1)?;
        assert!(matches!(
            machine.take_ui_debug_stop(),
            Some(crate::debugger::DebugStop::Watch {
                addr: 0x0400,
                access: crate::debugger::WatchAccess::Read,
                ..
            })
        ));
        assert!(machine.bus_mut().ui_take_mem_writer(0x0400).is_none());

        machine.step_slice(1)?;
        assert!(
            machine.take_ui_debug_stop().is_none(),
            "the TAS write must not surface as a stale second stop"
        );
        Ok(())
    }

    #[test]
    fn odd_byte_read_touches_only_the_word_that_contains_it() -> Result<()> {
        let pc = ROM_BASE as u32 + 0x0100;
        let program = &[0x1039, 0x0000, 0x0401]; // MOVE.B $00000401,D0

        let mut containing = machine_with_program(pc, program)?;
        containing.ui_toggle_watch_access(0x0400, None, None, crate::debugger::WatchAccess::Read);
        containing.step_slice(1)?;
        assert!(matches!(
            containing.take_ui_debug_stop(),
            Some(crate::debugger::DebugStop::Watch { addr: 0x0400, .. })
        ));

        let mut adjacent = machine_with_program(pc, program)?;
        adjacent.ui_toggle_watch_access(0x0402, None, None, crate::debugger::WatchAccess::Read);
        adjacent.step_slice(1)?;
        assert!(adjacent.take_ui_debug_stop().is_none());
        Ok(())
    }

    #[test]
    fn tas_chip_ram_byte_read_and_write_arbitrate_separately_under_blitter_dma() -> Result<()> {
        const BLITTER_DMA_ENABLE: u16 = 1 << 6;

        let mut machine = machine_with_program(
            ROM_BASE as u32 + 0x0100,
            &[0x4AF9, 0x0000, 0x0400], // TAS.B $00000400
        )?;

        {
            let bus = machine.bus_mut();
            bus.agnus.hpos = 0x20;
            bus.mem.chip_ram[0x0400] = 0x12;
            bus.agnus.dmacon = DMACON_DMAEN | BLITTER_DMA_ENABLE;
            bus.blitter.bltcon0 = 0x09F0;
            bus.blitter.bltafwm = 0xFFFF;
            bus.blitter.bltalwm = 0xFFFF;
            bus.blitter.bltapt = 0x0800;
            bus.blitter.bltdpt = 0x0840;
            for word in 0..16 {
                write_chip_word(bus, 0x0800 + word * 2, 0x1000 | word as u16);
            }
            bus.blitter
                .start_scheduled((1 << 6) | 16, &bus.mem.chip_ram);
            // Walk the blit past its two internal lead-in cycles (those are
            // CPU-available) so its pending slot is an A-channel bus access.
            bus.advance_chipset(2);
        }

        let initial_slots = machine.bus().blitter.scheduled_slots_remaining();
        let slice = machine.step_slice(1)?;

        // The instruction runs from ROM (TAS.B $00000400 = opcode + 2 address
        // words + a 2-word cold-start queue top-up = 5 external word accesses at
        // 2 cck each = 10 cck), then makes two contended chip-RAM byte accesses
        // (the read-modify-write). With BLITHOG clear the busy blitter holds the
        // chip bus and yields only after BLITTER_SLOWDOWN_CPU_MISS_LIMIT missed
        // cycles, so each chip access costs limit missed cycles + 1 granted slot
        // + 1 bus-free tail = limit + 2 cck.
        let rom_prefetch_cck = 5 * 2;
        let per_chip_access = u32::from(crate::bus::BLITTER_SLOWDOWN_CPU_MISS_LIMIT) + 2;
        // TAS spends 2 internal CPU clocks (1 cck) between the operand read
        // and the writeback half of its indivisible read-modify-write cycle
        // (yacht: np nrb n nw; Moira execTasEa SYNC(2)).
        let rmw_internal_cck = 1;
        assert_eq!(slice.instructions, 1);
        assert_eq!(machine.bus().mem.chip_ram[0x0400], 0x92);
        assert_eq!(
            slice.bus_advanced_cck,
            rom_prefetch_cck + 2 * per_chip_access + rmw_internal_cck,
            "TAS bus time = ROM prefetch (external, 2 cck/word) plus the two contended chip-RAM byte accesses (limit + 2 cck each under a busy BLITHOG-clear blitter) plus the 1 cck read-to-write internal gap"
        );
        assert!(machine.bus().blitter.busy);
        // The blitter held the bus through the CPU's starvation waits rather than
        // yielding its regular slots, so it advanced its scheduled work.
        assert!(machine.bus().blitter.scheduled_slots_remaining() < initial_slots);
        Ok(())
    }

    #[test]
    fn external_accesses_advance_bus_time_without_chip_slots() -> Result<()> {
        // External regions (fast RAM, slow RAM, ROM, CIA) no longer take a
        // contended chip-bus slot, but they DO advance bus time: each word
        // access costs the 68000's 4-clock bus cycle = 2 cck (via
        // Bus::cpu_external_access), with the chip bus left free for DMA. The
        // instruction is the same shape as the chip-RAM word data read
        // (run from ROM, 6 word accesses total: opcode + 2 address words + 1
        // data word + 2-word cold-start queue top-up), so it advances the bus
        // by 6 x 2 = 12 cck -- the same total as the chip case, just with no
        // chip-bus contention.
        for (label, address) in [
            ("fast RAM word data read", FAST_RAM_BASE as u32 + 0x0200),
            ("slow RAM word data read", SLOW_RAM_BASE as u32 + 0x0200),
            ("ROM word data read", ROM_BASE as u32 + 0x0200),
        ] {
            let hi = ((address >> 16) & 0xFFFF) as u16;
            let lo = (address & 0xFFFF) as u16;
            let slice = run_rom_instruction(&[0x3039, hi, lo])?;
            assert_single_instruction_timing(label, slice, 8, 12);
        }
        // A CIA access is a 6800-style VPA cycle: the data transfer waits
        // for the E clock (Bus::cpu_cia_access), adding a phase-dependent
        // 3..7 cck on top of the plain external cost. The phase here is
        // fixed by the test's deterministic start time.
        let hi = ((CIA_A_BASE >> 16) & 0xFFFF) as u16;
        let lo = (CIA_A_BASE & 0xFFFF) as u16;
        let slice = run_rom_instruction(&[0x3039, hi, lo])?;
        assert_single_instruction_timing("CIA word data read", slice, 8, 15);
        Ok(())
    }

    #[test]
    fn accelerated_cpu_scales_fast_ram_but_not_chip_ram_time() -> Result<()> {
        // [cpu] clock_mhz = 100 -> 28 CPU clocks per cck (vs the stock 2).
        // Fast RAM is off the chip bus, so a fast-RAM NOP stream (1 word fetch
        // = 4 CPU clocks each) must complete in clock-ratio-proportionally
        // fewer cck. Chip RAM stays chip-bus bound: one contended slot per
        // word no matter how fast the CPU is clocked.
        //
        // Regression: cpu_external_access billed a fixed 2 cck/word and the
        // per-instruction cck conversion rounded up to a whole cck, so a
        // 99 MHz config still ran fetch-bound code at roughly stock speed.
        let nops = 280usize;
        let program: Vec<u16> = vec![0x4E71; nops + 4];

        let run = |pc: u32, clocks_per_cck: u32| -> Result<u32> {
            let mut bus = test_bus_with_pc(pc);
            write_program(&mut bus, pc, &program);
            let mut machine = build(
                bus,
                CpuModel::M68000,
                false,
                clocks_per_cck,
                Default::default(),
                false,
            )?;
            machine.bus_mut().agnus.hpos = 0x21;
            let mut total = 0u32;
            for _ in 0..nops {
                total += machine.step_slice(1)?.bus_advanced_cck;
            }
            Ok(total)
        };

        let fast_pc = FAST_RAM_BASE as u32 + 0x0100;
        let fast_stock = run(fast_pc, 2)?;
        let fast_accel = run(fast_pc, 28)?;
        // 282 word fetches (cold-start prefetch included): stock 2 cck each;
        // at 28 clocks/cck the same 4-clock bus cycles total 282*4/28 = ~40.
        assert_eq!(fast_stock, 564, "stock fast RAM NOP stream");
        assert!(
            (fast_accel as i64 - 40).abs() <= 1,
            "accelerated fast RAM NOP stream should take ~40 cck, got {fast_accel}"
        );

        let chip_pc = 0x0000_0100u32;
        let chip_stock = run(chip_pc, 2)?;
        let chip_accel = run(chip_pc, 28)?;
        assert_eq!(
            chip_accel, chip_stock,
            "chip RAM code is chip-bus bound regardless of CPU clock"
        );
        Ok(())
    }

    #[test]
    fn cpu_bus_timing_charges_custom_register_word_bus_cycles() -> Result<()> {
        // Custom-register ($DFFxxx) accesses are now routed through the
        // contended chip bus (grant_cpu_bus_access with CpuBusAccessKind::Custom)
        // like chip-RAM accesses: 2 cck per word plus any contention waits. The
        // opcode and address prefetches come from ROM (external, 2 cck/word).
        //
        // Custom accesses pay the same chip-bus cost as chip-RAM accesses
        // (custom_read/custom_write grant the slot exactly once).
        assert_single_instruction_timing(
            "custom register word read",
            run_rom_instruction(&[0x3039, 0x00DF, 0xF002])?,
            8,
            12,
        );
        assert_single_instruction_timing(
            "custom register long read",
            run_rom_instruction(&[0x2039, 0x00DF, 0xF002])?,
            10,
            14,
        );
        assert_single_instruction_timing(
            "custom register word write",
            run_rom_instruction(&[0x33FC, 0x0123, 0x00DF, 0xF180])?,
            10,
            14,
        );
        assert_single_instruction_timing(
            "custom register long write",
            run_rom_instruction(&[0x23FC, 0x0000, 0x0123, 0x00DF, 0xF180])?,
            14,
            18,
        );
        Ok(())
    }

    #[test]
    fn cpu_prefetch_probe_documents_self_modified_next_opcode_behavior() -> Result<()> {
        let mut machine = machine_with_program(
            0x0000_0100,
            &[
                0x31FC, 0x7002, 0x0106, // MOVE.W #$7002,$0106
                0x7001, // MOVEQ #1,D0, overwritten before execution.
            ],
        )?;

        let slice = machine.step_slice(2)?;

        assert_eq!(slice.instructions, 2);
        assert_eq!(
            machine.d(0),
            1,
            "the MC68000 prefetch queue (m68k Part E.1) fetched the next opcode before the MOVE's write landed, so the stale MOVEQ #1 executes -- real 68000 Class 1 self-modifying-code behavior"
        );
        assert_eq!(&machine.bus().mem.chip_ram[0x0106..0x0108], &[0x70, 0x02]);
        Ok(())
    }

    #[test]
    fn cpu_prefetch_probe_branch_refetches_self_modified_chip_ram_target() -> Result<()> {
        let mut machine = machine_with_program(
            0x0000_0100,
            &[
                0x31FC, 0x7002, 0x010A, // MOVE.W #$7002,$010A
                0x6002, // BRA.S $010A, flushing a real 68000 prefetch queue.
                0x4E71, // skipped filler
                0x7001, // MOVEQ #1,D0, branch target overwritten before fetch.
            ],
        )?;

        let slice = machine.step_slice(3)?;

        assert_eq!(slice.instructions, 3);
        assert_eq!(
            machine.d(0),
            2,
            "branch-target self-modifying code is observed after the control-flow change, matching MC68000 prefetch-flush expectations"
        );
        assert_eq!(&machine.bus().mem.chip_ram[0x010A..0x010C], &[0x70, 0x02]);
        Ok(())
    }

    // 68020+ cache / CACR characterization. The m68k backend models perfectly
    // coherent, zero-latency memory: MOVEC accepts and stores CACR/CAAR but the
    // cache-control bits have no effect, CINV/CPUSH are NOPs, and there is no
    // instruction/data cache, prefetch queue, or write buffer. These tests pin
    // that backend behavior. (The opt-in icache/dcache models in src/cache.rs
    // sit above this coherent core; see docs/internals/cpu.md "Caches".)

    #[test]
    fn movec_to_cacr_round_trips_on_68020() -> Result<()> {
        // MOVEQ #$0F,D0 ; MOVEC D0,CACR ; MOVEC CACR,D1
        let mut machine = machine_with_program_model(
            0x0000_0100,
            &[0x700F, 0x4E7B, 0x0002, 0x4E7A, 0x1002],
            CpuModel::M68020,
        )?;

        machine.step_slice(3)?;

        // The enable/freeze bits store and read back; the clear bits
        // (CEI/CI, bits 2/3) are write-only strobes that read back as
        // zero, as on real silicon.
        assert_eq!(machine.cpu.cacr, 0x03);
        assert_eq!(machine.d(1), 0x03);
        Ok(())
    }

    #[test]
    fn movec_is_illegal_on_68000() -> Result<()> {
        // The 68000 has no control registers; MOVEC traps as an illegal
        // instruction (vector 4, offset 0x10).
        let mut bus = test_bus_with_pc(0x0000_0100);
        write_program(&mut bus, 0x0000_0100, &[0x4E7B, 0x0002]); // MOVEC D0,CACR
        write_chip_long(&mut bus, 0x10, 0x0000_0200); // illegal-instruction vector
        write_program(&mut bus, 0x0000_0200, &[0x4E71]); // handler NOP
        let mut machine = M68kMachine::new(bus, CpuModel::M68000, false)?;
        machine.bus_mut().agnus.hpos = 0x21;

        machine.step_slice(1)?;

        assert_eq!(machine.pc(), 0x0000_0200);
        Ok(())
    }

    #[test]
    fn movec_privilege_violation_in_user_mode_on_68020() -> Result<()> {
        // MOVE #$0000,SR drops to user mode; MOVEC is supervisor-only, so the
        // following MOVEC traps to the privilege-violation vector (8, 0x20).
        let mut bus = test_bus_with_pc(0x0000_0100);
        write_program(&mut bus, 0x0000_0100, &[0x46FC, 0x0000, 0x4E7B, 0x0801]); // MOVE #0,SR ; MOVEC D0,VBR
        write_chip_long(&mut bus, 0x20, 0x0000_0200);
        write_program(&mut bus, 0x0000_0200, &[0x4E71]);
        let mut machine = M68kMachine::new(bus, CpuModel::M68020, false)?;
        machine.bus_mut().agnus.hpos = 0x21;

        machine.step_slice(2)?;

        assert_eq!(machine.pc(), 0x0000_0200);
        // The exception returned the CPU to supervisor mode.
        assert_ne!(machine.sr() & 0x2000, 0);
        Ok(())
    }

    #[test]
    fn enabling_instruction_cache_does_not_stale_self_modified_code() -> Result<()> {
        // Enable the 68020 instruction cache via CACR, then overwrite the next
        // opcode. A real 68020 with the I-cache enabled could execute the stale
        // cached MOVEQ #1 until a CINV; the coherent backend always runs the
        // freshly written MOVEQ #2. This is the one real fidelity difference,
        // and it is the safe direction (correct software flushes with CINV).
        let mut machine = machine_with_program_model(
            0x0000_0100,
            &[
                0x7001, // MOVEQ #1,D0
                0x4E7B, 0x0002, // MOVEC D0,CACR (enable instruction cache)
                0x31FC, 0x7202, 0x010C, // MOVE.W #$7202,$010C  (-> MOVEQ #2,D1)
                0x7201, // MOVEQ #1,D1 at $010C, overwritten before execution
            ],
            CpuModel::M68020,
        )?;

        machine.step_slice(4)?;

        assert_eq!(machine.cpu.cacr, 1, "instruction cache enable bit latched");
        assert_eq!(
            machine.d(1),
            2,
            "self-modified code is seen immediately; the backend has no stale instruction cache"
        );
        Ok(())
    }

    #[test]
    fn cinv_takes_line_f_on_68030() -> Result<()> {
        // CINV/CPUSH are 68040 instructions; the 68030 (whose cache control
        // is CACR-based) raises Line-F for them (cputest-verified).
        let mut bus = test_bus_with_pc(0x0000_0100);
        write_program(&mut bus, 0x0000_0100, &[0xF4D8]); // CINVA BC
        write_chip_long(&mut bus, 0x0000_002C, 0x0000_0200); // Line-F vector
        write_program(&mut bus, 0x0000_0200, &[0x7007]); // MOVEQ #7,D0
        let mut machine = M68kMachine::new(bus, CpuModel::M68030, false)?;
        machine.bus_mut().agnus.hpos = 0x21;

        let slice = machine.step_slice(2)?;

        assert!(!slice.stopped);
        assert_eq!(machine.d(0), 7, "Line-F handler must run");
        assert_eq!(machine.pc(), 0x0000_0202);
        Ok(())
    }

    #[test]
    fn movec_sets_vbr_and_redirects_exception_vectors_on_68020() -> Result<()> {
        // MOVEC to VBR relocates the exception vector table; TRAP #1 must fetch
        // its handler from the new base (VBR + 0x84).
        let mut bus = test_bus_with_pc(0x0000_0100);
        write_program(
            &mut bus,
            0x0000_0100,
            &[0x203C, 0x0000, 0x0400, 0x4E7B, 0x0801, 0x4E41], // MOVE.L #$400,D0 ; MOVEC D0,VBR ; TRAP #1
        );
        write_chip_long(&mut bus, 0x0000_0484, 0x0000_0200); // VBR(0x400) + TRAP#1 vector (0x84)
        write_program(&mut bus, 0x0000_0200, &[0x4E71]);
        let mut machine = M68kMachine::new(bus, CpuModel::M68020, false)?;
        machine.bus_mut().agnus.hpos = 0x21;

        machine.step_slice(3)?;

        assert_eq!(machine.cpu.vbr, 0x0000_0400);
        assert_eq!(machine.pc(), 0x0000_0200);
        Ok(())
    }

    #[test]
    fn subq_long_sets_zero_for_signed_bgt_loop_exit() -> Result<()> {
        let mut machine = machine_with_program(
            0x00FC_00D2,
            &[
                0x203C, 0x0002, 0x0000, // MOVE.L #$20000,D0
                0x5380, // SUBQ.L #1,D0
                0x6EFC, // BGT.S -4
                0x4E71, // NOP
            ],
        )?;

        let slice = machine.step_slice(262_145)?;

        assert!(!slice.stopped);
        assert_eq!(machine.d(0), 0);
        assert_eq!(machine.pc(), 0x00FC_00DC);
        assert_ne!(machine.sr() & 0x0004, 0);
        Ok(())
    }

    /// A stub freezer-cartridge image: `HRT!` at +4 and, at the level-7
    /// entry (+12), a handler that writes a marker word into chip RAM and
    /// returns: MOVE.W #$1234,($1000).W ; RTE.
    fn stub_cartridge() -> crate::cartridge::Cartridge {
        let mut image = vec![0u8; 0x60];
        image[4..8].copy_from_slice(b"HRT!");
        for (i, word) in [0x31FCu16, 0x1234, 0x1000, 0x4E73].iter().enumerate() {
            let at = crate::cartridge::HRTMON_ENTRY_OFFSET as usize + i * 2;
            image[at..at + 2].copy_from_slice(&word.to_be_bytes());
        }
        crate::cartridge::Cartridge::hrtmon(&image).unwrap()
    }

    #[test]
    fn cartridge_freeze_delivers_one_level7_interrupt_through_the_written_vector() -> Result<()> {
        use crate::cartridge::{HRTMON_BASE, HRTMON_CUSTOM_SHADOW, HRTMON_ENTRY_OFFSET};
        let mut bus = test_bus_with_pc(0x0000_0100);
        // Mask every level Paula can raise, set COLOR00 (a write the
        // cartridge shadows), then idle in a two-NOP loop the freeze
        // interrupts.
        write_program(
            &mut bus,
            0x0000_0100,
            &[
                0x46FC, 0x2700, // MOVE #$2700,SR
                0x33FC, 0x0F00, 0x00DF, 0xF180, // MOVE.W #$0F00,$DFF180
                0x4E71, // $10C: NOP
                0x4E71, // $10E: NOP
                0x60FA, // $110: BRA.S $10C
            ],
        );
        bus.attach_cartridge(stub_cartridge());
        // The delivery mechanism, not the recognition latency (exercised by
        // the timing-test ROM): immediate IPL like the Paula test below.
        bus.irq_latency_setting = 0;
        let mut machine = M68kMachine::new(bus, CpuModel::M68000, false)?;
        machine.step_slice(4)?; // SR, COLOR00, NOP, NOP
        assert_eq!(machine.pc(), 0x0000_0110);
        assert_eq!(machine.sr() & 0x0700, 0x0700);
        assert!(!machine.bus().cartridge_nmi_pending());

        // The button: the vector under VBR (0 on a 68000) now names the
        // cartridge entry, the shadows are in the bank, the interrupt waits.
        let entry = machine.cartridge_freeze().unwrap();
        assert_eq!(entry, HRTMON_BASE + HRTMON_ENTRY_OFFSET);
        assert_eq!(read_chip_long(machine.bus(), 0x7C), entry);
        assert!(machine.bus().cartridge_nmi_pending());
        let bank = machine.bus().cartridge.as_ref().unwrap().bank();
        assert_eq!(
            &bank[HRTMON_CUSTOM_SHADOW + 0x180..HRTMON_CUSTOM_SHADOW + 0x182],
            &[0x0F, 0x00],
            "the COLOR00 write reached the shadow the monitor reads"
        );

        // Taken at the next boundary despite the mask of 7: the handler in
        // the bank ran (the marker landed), RTE brought the loop back with
        // its SR, and the request was consumed at the acknowledge.
        let slice = machine.step_slice(3)?; // MOVE.W, RTE, BRA
        assert_eq!(slice.instructions, 3);
        assert_eq!(read_chip_word(machine.bus(), 0x1000), 0x1234);
        assert_eq!(machine.pc(), 0x0000_010C);
        assert_eq!(machine.sr() & 0x0700, 0x0700, "RTE restored the mask");
        assert!(!machine.bus().cartridge_nmi_pending());
        assert_eq!(machine.bus().cartridge.as_ref().unwrap().freezes(), 1);

        // Level 7 is recognised on its transition, not its level: nothing
        // re-enters the handler until the button is pressed again.
        machine.bus_mut().mem.chip_ram[0x1000..0x1002].copy_from_slice(&[0, 0]);
        machine.step_slice(30)?;
        assert_eq!(read_chip_word(machine.bus(), 0x1000), 0);
        machine.cartridge_freeze().unwrap();
        machine.step_slice(3)?;
        assert_eq!(read_chip_word(machine.bus(), 0x1000), 0x1234);
        assert_eq!(machine.bus().cartridge.as_ref().unwrap().freezes(), 2);
        Ok(())
    }

    #[test]
    fn paula_irq_autovector_acknowledge_stacks_68000_frame() -> Result<()> {
        let mut bus = test_bus_with_pc(0x0000_0100);
        write_program(&mut bus, 0x0000_0100, &[0x46FC, 0x2000]); // MOVE #$2000,SR
        write_program(&mut bus, 0x0000_0200, &[0x4E71]); // IRQ handler NOP
        set_autovector(&mut bus, 3, 0x0000_0200);
        bus.paula.intena = INT_MASTER | INT_VERTB;
        bus.paula.intreq = INT_VERTB;
        // Test the autovector MECHANISM with immediate delivery; the recognition
        // latency (default on) is exercised by the timing-test ROM, not here.
        bus.irq_latency_setting = 0;

        let mut machine = M68kMachine::new(bus, CpuModel::M68000, false)?;
        let slice = machine.step_slice(2)?;

        assert_eq!(slice.instructions, 2);
        assert_eq!(machine.pc(), 0x0000_0202);
        assert_eq!(machine.a(7), 0x0007_FFF8);
        assert_eq!(read_chip_word(machine.bus(), machine.a(7)), 0x2000);
        assert_eq!(read_chip_long(machine.bus(), machine.a(7) + 2), 0x0000_0104);
        assert_eq!(machine.bus().delivered_irq_pending, INT_VERTB);
        Ok(())
    }

    /// The JIT slice must produce the same architectural result as the
    /// precise slice for plain computation: a summing loop in fast RAM
    /// ends with identical registers and PC. The 68020 exercises the
    /// batch path (through the fastmem window); the 68000 documents the
    /// precise fallback (its shared-bus float model rules the batch
    /// contract out, see `jit_fast_path_ok`).
    #[test]
    fn jit_slice_matches_precise_registers_and_pc() -> Result<()> {
        let program = [
            0x7000u16, // moveq #0,d0
            0x7231,    // moveq #49,d1
            0xD081,    // add.l d1,d0
            0x51C9, 0xFFFC, // dbra d1,<add>
            0x60FE, // bra.s *
        ];
        let pc = FAST_RAM_BASE as u32 + 0x100;
        for model in [CpuModel::M68000, CpuModel::M68020, CpuModel::M68030] {
            let run = |jit: bool| -> Result<(u32, u32, u32)> {
                let mut machine = machine_with_program_model(pc, &program, model)?;
                machine.set_jit_enabled(jit);
                let mut retired = 0usize;
                // 2 setup + 100 loop instructions; the final bra self-loop
                // absorbs whatever remains of the budget.
                while retired < 400 {
                    retired += machine.step_slice(400 - retired)?.instructions;
                }
                Ok((machine.d(0), machine.d(1), machine.pc()))
            };
            let precise = run(false)?;
            let jit = run(true)?;
            assert_eq!(precise, jit, "{model:?}");
            assert_eq!(jit.0, 1225, "{model:?}");
            assert_eq!(jit.2, pc + 10, "{model:?}");
        }
        Ok(())
    }

    /// Traps surfaced by the batch path (which never takes them itself)
    /// must be raised as real hardware exceptions, mirroring the precise
    /// path's no-op HLE policy.
    #[test]
    fn jit_slice_takes_trap_exceptions() -> Result<()> {
        let pc = FAST_RAM_BASE as u32 + 0x100;
        let mut bus = test_bus_with_pc(pc);
        write_program(&mut bus, pc, &[0x4E40]); // trap #0
        write_program(&mut bus, 0x0000_0200, &[0x702A, 0x60FE]); // moveq #42,d0; bra.s *
        write_chip_long(&mut bus, 0x80, 0x0000_0200); // TRAP #0 vector
        let mut machine = M68kMachine::new(bus, CpuModel::M68020, true)?;
        machine.set_jit_enabled(true);
        machine.step_slice(8)?;
        assert_eq!(machine.d(0), 42);
        assert_eq!(machine.pc(), 0x0000_0202);
        Ok(())
    }

    /// A device interrupt raised while the JIT spins inside the fastmem
    /// window (no bus accesses at all) is still recognized at the next
    /// slice boundary and its autovector exception taken.
    #[test]
    fn jit_slice_delivers_device_interrupts() -> Result<()> {
        let pc = FAST_RAM_BASE as u32 + 0x100;
        let mut bus = test_bus_with_pc(pc);
        write_program(&mut bus, pc, &[0x46FC, 0x2000, 0x60FE]); // move #$2000,SR; bra.s *
        write_program(&mut bus, 0x0000_0200, &[0x702A, 0x60FE]); // moveq #42,d0; bra.s *
        set_autovector(&mut bus, 3, 0x0000_0200);
        bus.paula.intena = INT_MASTER | INT_VERTB;
        let mut machine = M68kMachine::new(bus, CpuModel::M68020, true)?;
        machine.set_jit_enabled(true);
        machine.step_slice(64)?;
        assert_eq!(machine.pc(), pc + 4, "spinning with no interrupt pending");
        machine.bus_mut().paula.intreq = INT_VERTB;
        machine.step_slice(64)?;
        assert_eq!(machine.d(0), 42, "handler must have run");
        Ok(())
    }

    /// Every CPU-level diagnostic observer is serviced only by the precise
    /// loop, the single-word CPU write watch included: arming one on a JIT
    /// machine must take it off the batch path, and it must count as an
    /// armed hook for the fallback warning.
    #[test]
    fn jit_memw_hook_forces_the_precise_loop() -> Result<()> {
        let pc = FAST_RAM_BASE as u32 + 0x100;
        let mut bus = test_bus_with_pc(pc);
        write_program(&mut bus, pc, &[0x60FE]); // bra.s *
        let mut machine = M68kMachine::new(bus, CpuModel::M68020, true)?;
        machine.set_jit_enabled(true);
        assert!(!machine.debug_hooks_armed());
        assert!(machine.jit_fast_path_ok());
        machine.arm_diagnostic_hooks(DiagnosticHooks {
            memw_addr: Some(0x0000_0400),
            ..DiagnosticHooks::default()
        });
        assert!(machine.debug_hooks_armed());
        assert!(!machine.jit_fast_path_ok());
        machine.arm_diagnostic_hooks(DiagnosticHooks::default());
        assert!(
            machine.jit_fast_path_ok(),
            "disarming restores the batch path"
        );
        Ok(())
    }

    /// An FPU-less 68040 always runs the precise loop (the host shim covers
    /// its missing FPU), which is a model property and not an armed hook:
    /// the JIT fallback warning must not claim a debugger changed the
    /// timeline of an undebugged run.
    #[test]
    fn fpu_less_68040_shim_is_not_a_debug_hook() -> Result<()> {
        let pc = FAST_RAM_BASE as u32 + 0x100;
        let mut bus = test_bus_with_pc(pc);
        write_program(&mut bus, pc, &[0x60FE]); // bra.s *
        let mut machine = M68kMachine::new(bus, CpuModel::M68040, false)?;
        machine.set_jit_enabled(true);
        assert!(
            !machine.jit_fast_path_ok(),
            "the FPU shim keeps the precise loop"
        );
        assert!(!machine.debug_hooks_armed());
        Ok(())
    }

    /// A chip-RAM-resident loop under JIT must run at CPU speed once the
    /// instruction-cache model holds it, not pay chip-bus arbitration on
    /// every fetch: bypassing the cache models made a fast-RAM-less
    /// Workbench measure 3x SLOWER under JIT than under the precise core
    /// (SysInfo Dhrystones 1667 vs 5503 on a 50 MHz 68040).
    #[test]
    fn jit_chip_resident_loop_runs_at_cpu_speed_with_icache() -> Result<()> {
        let program = [
            0x7001u16, // moveq #1,d0 (CACR.EI, as the OS sets at boot)
            0x4E7B, 0x0002, // movec d0,cacr
            0x7263, // moveq #99,d1
            0x4E71, // nop
            0x51C9, 0xFFFC, // dbra d1,<nop>
            0x60FE, // bra.s *
        ];
        let run = |icache: bool| -> Result<u32> {
            let mut machine = machine_with_program_model(0x1000, &program, CpuModel::M68020)?;
            machine.set_cache_emulation(icache, false);
            machine.set_jit_enabled(true);
            let mut retired = 0usize;
            let mut device_cck = 0u32;
            while retired < 220 {
                let slice = machine.step_slice(220 - retired)?;
                retired += slice.instructions;
                device_cck += slice.bus_advanced_cck;
            }
            Ok(device_cck)
        };
        let cached = run(true)?;
        let cacheless = run(false)?;
        assert!(
            cached < 300,
            "cached chip-resident loop took {cached} cck for 220 instructions"
        );
        assert!(
            cacheless > cached * 2,
            "cache model should dominate chip-loop speed: cacheless {cacheless} vs cached {cached}"
        );
        Ok(())
    }

    /// The fastmem window is only offered while nothing intercepts plain
    /// fast-RAM accesses, and covers the largest fitted fast bank.
    #[test]
    fn jit_fast_mem_window_gates_and_bank_choice() -> Result<()> {
        let mut machine = machine_with_program_model(0x0100, &[0x4E71], CpuModel::M68020)?;
        // Not offered until JIT mode is on.
        assert!(machine.bus.jit_fast_mem().is_none());
        machine.set_jit_enabled(true);
        let window = machine.bus.jit_fast_mem().expect("zorro fast RAM window");
        assert_eq!(window.base, FAST_RAM_BASE as u32);
        assert_eq!(window.len, 64 * 1024);
        // A memory-write debug hook must force every access back onto the
        // bus so it keeps seeing them.
        machine.bus.dbg_memw_addr = Some(0x1234);
        assert!(machine.bus.jit_fast_mem().is_none());
        machine.bus.dbg_memw_addr = None;
        // A bigger motherboard bank wins.
        machine.bus.bus.mem.fit_mb_ram(2 * 1024 * 1024);
        let window = machine.bus.jit_fast_mem().expect("motherboard window");
        assert_eq!(window.base, 0x07E0_0000);
        assert_eq!(window.len, 2 * 1024 * 1024);
        Ok(())
    }

    /// A 68000/68010 must never get a fastmem window: on the small-box
    /// machines every CPU cycle drives the one bus shared with Agnus, and
    /// the floating-bus latch (`note_cpu_data_bus`) has to see each
    /// transfer -- Kickstart/AROS memory sizing reads those floats, and
    /// bypassing the latch wedges the boot.
    #[test]
    fn jit_fast_mem_window_never_offered_on_shared_bus_cpus() -> Result<()> {
        for model in [CpuModel::M68000, CpuModel::M68010] {
            let mut machine = machine_with_program_model(0x0100, &[0x4E71], model)?;
            machine.set_jit_enabled(true);
            assert!(machine.bus.jit_fast_mem().is_none(), "{model:?}");
        }
        Ok(())
    }

    #[test]
    fn stopped_cpu_wakes_when_cia_timer_raises_device_interrupt() -> Result<()> {
        let mut bus = test_bus_with_pc(0x0000_0100);
        write_program(&mut bus, 0x0000_0100, &[0x4E72, 0x2000]); // STOP #$2000
        write_program(&mut bus, 0x0000_0200, &[0x4E71]); // IRQ handler NOP
        set_autovector(&mut bus, 2, 0x0000_0200);
        bus.paula.intena = INT_MASTER | INT_PORTS;
        // Keep this a mechanism test: once the CIA line is asserted, expose it
        // immediately. IRQ recognition latency has separate timing coverage.
        bus.irq_latency_setting = 0;

        let mut machine = M68kMachine::new(bus, CpuModel::M68000, false)?;
        let stopped = machine.step_slice(1)?;
        assert_eq!(stopped.instructions, 1);
        assert!(machine.stopped());
        assert_eq!(machine.pc(), 0x0000_0104);

        // Arm CIA-A timer A only after STOP has retired, then advance device
        // time just as the emulator's stopped-CPU idle path does.
        let _ = machine.bus_mut().cia_a.write(REG_TALO, 1);
        let _ = machine.bus_mut().cia_a.write(REG_TAHI, 0);
        let _ = machine.bus_mut().cia_a.write(REG_ICR, 0x80 | 0x01);
        let _ = machine.bus_mut().cia_a.write(REG_CRA, 0x11);
        for _ in 0..64 {
            machine.bus_mut().advance_devices(1);
            if machine.bus().paula.intreq & INT_PORTS != 0 {
                break;
            }
        }
        assert_ne!(
            machine.bus().paula.intreq & INT_PORTS,
            0,
            "CIA timer must raise Paula PORTS during the idle advance"
        );

        machine.refresh_irq_line();
        let wake = machine.step_slice(1)?;

        assert!(!wake.stopped);
        assert!(!machine.stopped());
        assert_eq!(machine.pc(), 0x0000_0202);
        assert_eq!(machine.a(7), 0x0007_FFF8);
        assert_eq!(read_chip_word(machine.bus(), machine.a(7)), 0x2000);
        assert_eq!(read_chip_long(machine.bus(), machine.a(7) + 2), 0x0000_0104);
        assert_eq!(machine.bus().delivered_irq_pending & INT_PORTS, INT_PORTS);
        Ok(())
    }

    #[test]
    fn irq_bus_event_is_stamped_before_exception_bus_cycles() -> Result<()> {
        let mut bus = test_bus_with_pc(0x0000_0100);
        write_program(&mut bus, 0x0000_0200, &[0x4E71]);
        set_autovector(&mut bus, 2, 0x0000_0200);
        bus.paula.intena = INT_MASTER | INT_PORTS;
        bus.paula.intreq = INT_PORTS;
        bus.irq_latency_setting = 0;

        let mut machine = M68kMachine::new(bus, CpuModel::M68000, false)?;
        machine.cpu.set_sr(0x2000);
        machine.bus_mut().agnus.hpos = 0x20;
        machine.bus_mut().set_frame_analyzer_full(true);
        machine.bus_mut().set_cpu_bus_arbitration_enabled(true);
        let start = (machine.bus().agnus.vpos, machine.bus().agnus.hpos);

        assert!(machine.service_pending_irq_cycles().is_some());

        let record = machine
            .bus()
            .frame_bus_trace()
            .unwrap()
            .record_at(start.0 as usize, start.1 as usize)
            .unwrap();
        assert_ne!(record.events & crate::bus::BUS_EVENT_CPU_IRQ, 0);
        assert_eq!(record.ipl, 2);
        assert_ne!(
            (machine.bus().agnus.vpos, machine.bus().agnus.hpos),
            start,
            "exception dispatch must consume bus cycles after recognition"
        );
        Ok(())
    }

    #[cfg(feature = "control")]
    #[test]
    fn irq_profile_snapshot_precedes_exception_frame_and_handler_state() -> Result<()> {
        let mut bus = test_bus_with_pc(0x0000_0100);
        write_program(&mut bus, 0x0000_0100, &[0x4E71]);
        write_program(&mut bus, 0x0000_0200, &[0x4E71]);
        set_autovector(&mut bus, 2, 0x0000_0200);
        bus.paula.intena = INT_MASTER | INT_PORTS;
        bus.paula.intreq = INT_PORTS;
        bus.irq_latency_setting = 0;

        let mut machine = M68kMachine::new(bus, CpuModel::M68000, false)?;
        machine.cpu.set_sr(0x2000);
        machine.cpu.set_d(0, 0x1234_5678);
        let pre_irq_sp = machine.a(7);
        machine.start_profile_samples(None, Vec::new(), true);
        machine.step_slice(1)?;

        let samples = machine.take_profile_samples();
        let irq = samples.iter().find(|sample| sample.irq.is_some()).unwrap();
        let registers = irq.registers.expect("IRQ registers");
        assert_eq!(registers[0], 0x1234_5678);
        assert_eq!(
            registers[15], pre_irq_sp,
            "A7 must precede the exception frame"
        );
        assert_eq!(registers[16], 0x2000, "SR must precede the raised IPL mask");
        Ok(())
    }

    #[cfg(feature = "control")]
    #[test]
    fn short_bus_profile_accounts_for_granted_transfer_time() -> Result<()> {
        // MOVE.W D0,(A0) fetches and writes through the 020's chip-bus port.
        let mut machine = machine_with_program_model(0x100, &[0x3080], CpuModel::M68020)?;
        machine.cpu.set_a(0, 0x200);
        machine.start_profile_samples(None, Vec::new(), false);
        let slice = machine.step_slice(1)?;
        let samples = machine.take_profile_samples();
        assert_eq!(samples.len(), 1);
        let sample = &samples[0];
        assert_eq!(sample.total_cck, slice.bus_advanced_cck);
        assert_eq!(
            sample.instruction_cck + sample.bus_wait_cck,
            sample.total_cck
        );
        assert!(
            sample.instruction_cck > 0,
            "granted clocks must be retained"
        );
        Ok(())
    }

    #[test]
    fn paula_irq_ipl_mask_blocks_equal_level_autovector() -> Result<()> {
        let mut bus = test_bus_with_pc(0x0000_0100);
        write_program(
            &mut bus,
            0x0000_0100,
            &[
                0x46FC, 0x2300, // MOVE #$2300,SR keeps IPL3 masked.
                0x7011, // MOVEQ #$11,D0
            ],
        );
        write_program(&mut bus, 0x0000_0200, &[0x7022]); // Would run on IRQ.
        set_autovector(&mut bus, 3, 0x0000_0200);
        bus.paula.intena = INT_MASTER | INT_VERTB;
        bus.paula.intreq = INT_VERTB;

        let mut machine = M68kMachine::new(bus, CpuModel::M68000, false)?;
        let slice = machine.step_slice(2)?;

        assert_eq!(slice.instructions, 2);
        assert_eq!(machine.d(0), 0x11);
        assert_eq!(machine.pc(), 0x0000_0106);
        assert_eq!(machine.bus().delivered_irq_pending, 0);
        Ok(())
    }

    #[test]
    fn rollback_preserves_the_previous_instruction_interrupt_sample() -> Result<()> {
        for held in [false, true] {
            let mut bus = test_bus_with_pc(0x100);
            write_program(
                &mut bus,
                0x100,
                &[0x46fc, 0x2000, 0x4e71, 0x4e71, 0x4e71, 0x60fe],
            );
            write_program(&mut bus, 0x200, &[0x7007, 0x60fe]);
            set_autovector(&mut bus, 3, 0x200);
            bus.paula.intena = INT_MASTER | INT_VERTB;
            bus.irq_latency_setting = 5;
            let mut machine = M68kMachine::new(bus, CpuModel::M68000, false)?;
            machine.step_slice(1)?;
            machine.bus_mut().paula.intreq = INT_VERTB;
            machine.bus_mut().advance_devices(10);
            assert_eq!(machine.bus.sampled_irq_level, 0);
            machine.bus.ipl_sample_held = held;
            let mut checkpoint = Vec::new();
            machine.write_rollback_state(&mut checkpoint)?;
            machine.step_slice(1)?;
            assert_eq!(
                machine.pc(),
                0x106,
                "the old sample delays interrupt recognition"
            );
            let mut expected = Vec::new();
            machine.write_rollback_state(&mut expected)?;
            machine.step_slice(5)?;
            assert_eq!(machine.d(0), 7);
            machine.apply_rollback_state(&mut std::io::Cursor::new(&checkpoint))?;
            assert_eq!(machine.bus.sampled_irq_level, 0);
            assert_eq!(machine.bus.ipl_sample_held, held);
            machine.step_slice(1)?;
            let mut actual = Vec::new();
            machine.write_rollback_state(&mut actual)?;
            assert_eq!(
                actual, expected,
                "replay must take the interrupt on the same instruction"
            );
        }
        Ok(())
    }

    #[test]
    fn irq_recognition_uses_previous_instruction_ipl_sample() -> Result<()> {
        // The 68000 latches its IPL pins during bus cycles: a level that
        // rises AFTER an instruction's last bus access is not seen at the
        // next instruction boundary -- it is recognized one instruction
        // later, once a bus access has sampled the new level.
        let mut bus = test_bus_with_pc(0x0000_0100);
        write_program(
            &mut bus,
            0x0000_0100,
            &[
                0x46FC, 0x2000, // MOVE #$2000,SR opens the IPL mask
                0x4E71, 0x4E71, 0x4E71, 0x4E71, 0x4E71, 0x4E71, // NOP run
            ],
        );
        write_program(&mut bus, 0x0000_0200, &[0x7007]); // handler: MOVEQ #7,D0
        set_autovector(&mut bus, 3, 0x0000_0200);
        bus.paula.intena = INT_MASTER | INT_VERTB;
        // Non-zero setting = the IPL pipe (armed when the device tick sees
        // the poked level below) plus the boundary-sampling delay.
        bus.irq_latency_setting = 5;

        let mut machine = M68kMachine::new(bus, CpuModel::M68000, false)?;
        machine.bus_mut().agnus.hpos = 0x21;

        machine.step_slice(1)?; // MOVE to SR; samples see no pending level
        machine.bus_mut().paula.intreq = INT_VERTB; // level 3 rises post-sample

        machine.step_slice(1)?; // boundary sample is stale: must not be taken
        assert_eq!(
            machine.d(0),
            0,
            "IRQ must not be taken before the pipe expires and a bus access \
             samples the new level"
        );
        assert_eq!(machine.pc(), 0x0000_0106);

        // The 5-cck pipe expires within the next couple of 2-cck NOPs; the
        // first bus access after that samples the level and the following
        // boundary takes the interrupt.
        machine.step_slice(5)?;
        assert_eq!(
            machine.d(0),
            7,
            "handler must run once pipe + sample elapse"
        );
        Ok(())
    }

    #[test]
    fn paula_irq_rte_restores_status_and_return_pc() -> Result<()> {
        let mut bus = test_bus_with_pc(0x0000_0100);
        write_program(
            &mut bus,
            0x0000_0100,
            &[
                0x46FC, 0x2000, // MOVE #$2000,SR
                0x7001, // MOVEQ #1,D0 after RTE
            ],
        );
        write_program(
            &mut bus,
            0x0000_0200,
            &[
                0x33FC, INT_VERTB, 0x00DF, 0xF09C, // MOVE.W #INT_VERTB,INTREQ
                0x4E73, // RTE
            ],
        );
        set_autovector(&mut bus, 3, 0x0000_0200);
        bus.paula.intena = INT_MASTER | INT_VERTB;
        bus.paula.intreq = INT_VERTB;

        let mut machine = M68kMachine::new(bus, CpuModel::M68000, false)?;
        let slice = machine.step_slice(4)?;

        assert_eq!(slice.instructions, 4);
        assert_eq!(machine.d(0), 1);
        assert_eq!(machine.pc(), 0x0000_0106);
        assert_eq!(machine.a(7), 0x0007_FFFE);
        assert_eq!(machine.sr() & 0x2700, 0x2000);
        assert_eq!(machine.bus().paula.intreq & INT_VERTB, 0);
        Ok(())
    }

    #[test]
    fn cia_irq_pending_during_cpu_slice_autovectors_before_next_instruction() -> Result<()> {
        let mut bus = test_bus_with_pc(0x0000_0100);
        write_program(
            &mut bus,
            0x0000_0100,
            &[
                0x46FC, 0x2000, // MOVE #$2000,SR to unmask level-2 interrupts.
                // MOVE.L $0000.W,$0008.W: a chip-access-heavy instruction whose
                // bus traffic advances CIA E-clock time past the timer underflow.
                0x21F8, 0x0000, 0x0008,
                0x7011, // MOVEQ #$11,D0, skipped until after the level-2 handler.
            ],
        );
        write_program(&mut bus, 0x0000_0200, &[0x7055]); // MOVEQ #$55,D0
        set_autovector(&mut bus, 2, 0x0000_0200);
        bus.paula.intena = INT_MASTER | INT_PORTS;
        let _ = bus.cia_a.write(REG_TALO, 1);
        let _ = bus.cia_a.write(REG_TAHI, 0);
        let _ = bus.cia_a.write(REG_ICR, 0x80 | 0x01);
        let _ = bus.cia_a.write(REG_CRA, 0x11);
        // Mechanism test: immediate delivery (recognition latency tested by ROM).
        bus.irq_latency_setting = 0;

        let mut machine = M68kMachine::new(bus, CpuModel::M68000, false)?;
        machine.bus_mut().agnus.hpos = 0;
        machine.refresh_irq_line();
        // Slice: MOVE-to-SR, then the chip-access-heavy MOVE.L during which the
        // CIA timer underflows, then the level-2 handler's MOVEQ #$55 -- the
        // pending IRQ autovectors before MOVEQ #$11 can run.
        let slice = machine.step_slice(3)?;

        assert_eq!(slice.instructions, 3);
        assert_eq!(machine.d(0), 0x55);
        // The cycle-exact core keeps a two-word prefetch queue filled ahead of
        // the program counter, so after the handler's one-word MOVEQ #$55 at
        // $0200 the reported PC is $0200 + 2 (executed) + 4 (queued ahead).
        assert_eq!(machine.pc(), 0x0000_0206);
        assert_ne!(machine.bus().paula.intreq & INT_PORTS, 0);
        assert_eq!(machine.bus().delivered_irq_pending & INT_PORTS, INT_PORTS);
        Ok(())
    }

    #[test]
    fn cpu_reset_instruction_resets_external_devices_without_resetting_cpu() -> Result<()> {
        let pc = 0x0000_0100;
        let mut bus = test_bus_with_pc(pc);
        write_program(
            &mut bus,
            pc,
            &[
                0x4E70, // RESET
                0x4E71, // NOP prefetched before RESET restores the overlay.
            ],
        );

        let mut machine = M68kMachine::new(bus, CpuModel::M68000, false)?;
        {
            let bus = machine.bus_mut();
            bus.mem.chip_ram[4..8].copy_from_slice(&0x00C0_0276u32.to_be_bytes());
            bus.mem.chip_ram[0x1234] = 0xAA;
            bus.mem.overlay = false;
            bus.overlay_disable_pending = true;
            let _ = bus.cia_a.write(REG_DDRA, 0x03);
            let _ = bus.cia_a.write(REG_PRA, 0x00);
            bus.agnus.write_copcon(0x0002);
        }
        assert_ne!(machine.bus().agnus.copcon, 0);
        let start_cck = machine.bus().emulated_cck();
        let slice = machine.step_slice(1)?;

        assert_eq!(slice.instructions, 1);
        assert_eq!(machine.bus().agnus.copcon, 0);
        assert!(machine.bus().mem.overlay);
        assert!(!machine.bus().overlay_disable_pending);
        assert_eq!(machine.bus().cia_a.peek_register(REG_DDRA), 0);
        assert_eq!(machine.bus().cia_a.peek_register(REG_PRA), 0xC1);
        assert_eq!(machine.bus().mem.chip_ram[0x1234], 0xAA);
        assert!(machine.bus().emulated_cck() > start_cck);
        assert_eq!(machine.pc(), pc + 2);
        Ok(())
    }

    #[test]
    fn copper_irq_asserted_during_cpu_bus_wait_is_acknowledged_with_beam() -> Result<()> {
        let mut bus = test_bus_with_pc(0x0000_0100);
        write_program(
            &mut bus,
            0x0000_0100,
            &[
                0x46FC, 0x2000, // MOVE #$2000,SR while fetch waits behind Copper DMA.
                0x7011, // MOVEQ #$11,D0, skipped until after the level-3 handler.
            ],
        );
        write_program(&mut bus, 0x0000_0200, &[0x7066]); // MOVEQ #$66,D0
        write_program(
            &mut bus,
            0x0000_0300,
            &[
                0x009C,
                0x8000 | INT_COPER, // Copper MOVE INTREQ,SET|COPER
                0xFFFF,
                0xFFFE,
            ],
        );
        set_autovector(&mut bus, 3, 0x0000_0200);
        bus.paula.intena = INT_MASTER | INT_COPER;
        bus.agnus.dmacon = DMACON_DMAEN | DMACON_COPEN;
        bus.copper.jump(0x0000_0300);
        // Mechanism test: immediate delivery (recognition latency tested by ROM).
        bus.irq_latency_setting = 0;

        let mut machine = M68kMachine::new(bus, CpuModel::M68000, false)?;
        machine.bus_mut().agnus.hpos = 0x20;
        machine.refresh_irq_line();
        let slice = machine.step_slice(2)?;

        assert_eq!(slice.instructions, 2);
        assert_eq!(machine.d(0), 0x66);
        assert_eq!(machine.bus().delivered_irq_pending & INT_COPER, INT_COPER);
        // The Copper yields every other contended color clock to the waiting
        // CPU (its MOVE writes one register every 4 color clocks, leaving the
        // alternate cycle free). Starting at hpos 0x20 the Copper fetches its
        // MOVE's first word at 0x20, the CPU takes 0x21, and the Copper writes
        // INTREQ on its second-word fetch at 0x22.
        assert_eq!(machine.bus().delivered_copper_irq_beam, Some((0, 0x22)));
        Ok(())
    }

    #[test]
    fn save_state_round_trip_replays_identically() -> Result<()> {
        // A chip-RAM loop that increments a counter word and copies it to
        // COLOR00 each iteration, so CPU progress, chip-RAM contents, and
        // the beam-event capture all advance between snapshots.
        let mut bus = test_bus_with_pc(0x1000);
        write_program(
            &mut bus,
            0x1000,
            &[
                0x5279, 0x0000, 0x2000, // addq.w  #1,$2000.l
                0x33F9, 0x0000, 0x2000, 0x00DF, 0xF180, // move.w $2000.l,$DFF180.l
                0x60EE, // bra.s back to 0x1000
            ],
        );
        let mut machine = build(bus, CpuModel::M68000, false, 2, Default::default(), false)?;

        // Get past the first frame wraps so the per-frame capture buffers
        // hold both current- and last-frame contents when the state is taken.
        while machine.bus().emulated_frames() < 2 {
            machine.step_slice(1000)?;
        }

        let temp = std::env::temp_dir();
        let state_path = |name: &str| {
            temp.join(format!(
                "copperline-savestate-test-{}-{name}.clstate",
                std::process::id()
            ))
        };
        let state_t1 = state_path("t1");
        let state_t2 = state_path("t2");
        let state_t2_replay = state_path("t2-replay");
        let descriptor = crate::config::MachineDescriptor::default();

        crate::savestate::save(&machine, &descriptor, &state_t1)?;
        let run_trace = |machine: &mut M68kMachine| -> Result<Vec<(u32, u16, u64, u16)>> {
            let mut trace = Vec::new();
            for _ in 0..10 {
                machine.step_slice(2000)?;
                trace.push((
                    machine.pc(),
                    machine.sr(),
                    machine.bus().emulated_frames(),
                    read_chip_word(machine.bus(), 0x2000),
                ));
            }
            Ok(trace)
        };
        let original = run_trace(&mut machine)?;
        crate::savestate::save(&machine, &descriptor, &state_t2)?;
        // The loop actually ran: the counter advanced and frames elapsed.
        assert!(original.last().unwrap().3 > original.first().unwrap().3);

        // Rewind the same machine back to T1 and replay the same steps.
        crate::savestate::load(&mut machine, &state_t1)?;
        let replay = run_trace(&mut machine)?;
        crate::savestate::save(&machine, &descriptor, &state_t2_replay)?;

        assert_eq!(original, replay);
        // Byte-identical re-serialization is the strong check: every
        // serialized field of CPU, Bus, and chipset state round-tripped
        // and the replay diverged nowhere.
        assert_eq!(
            std::fs::read(&state_t2)?,
            std::fs::read(&state_t2_replay)?,
            "replayed state diverged from the original timeline"
        );

        for path in [&state_t1, &state_t2, &state_t2_replay] {
            let _ = std::fs::remove_file(path);
        }
        Ok(())
    }

    /// The reverse-debug snapshot ring reconstructs an earlier instruction
    /// boundary exactly (reverse step) and pins the last writer of a watched
    /// word, using the same chip-RAM counter loop as the save-state test.
    #[test]
    fn reverse_step_reconstructs_earlier_state_and_finds_last_writer() -> Result<()> {
        use crate::config::PacingBudget;
        use crate::emulator::Emulator;
        use crate::timetravel::ReverseOutcome;

        let mut bus = test_bus_with_pc(0x1000);
        // addq.w #1,$2000 ; move.w $2000,$DFF180 ; bra back. Only the addq
        // (at PC $1000) writes $2000, so the last writer is unambiguous.
        write_program(
            &mut bus,
            0x1000,
            &[
                0x5279, 0x0000, 0x2000, // addq.w  #1,$2000.l
                0x33F9, 0x0000, 0x2000, 0x00DF, 0xF180, // move.w $2000.l,$DFF180.l
                0x60EE, // bra.s   $1000
            ],
        );
        let mut emu = Emulator::new(
            bus,
            CpuModel::M68000,
            false,
            Default::default(),
            PacingBudget::Instructions,
            2,
            false,
        )?;
        emu.enable_time_travel(256, 1);

        // Run a handful of frames so the ring holds several snapshots.
        while emu.bus().emulated_frames() < 6 {
            emu.step_frame()?;
        }

        let pos_now = emu.retired_instructions();
        let sample = |emu: &Emulator| {
            (
                emu.machine.pc(),
                emu.machine.sr(),
                emu.bus().emulated_frames(),
                emu.bus().emulated_cck(),
                emu.bus().peek_word_any(0x2000),
            )
        };
        let here = sample(&emu);
        assert!(here.4 > 0, "the counter loop actually ran");

        // Step back 100 instructions, then replay forward to the original
        // position: the reconstructed state must match exactly.
        match emu.tt_reverse_step(100)? {
            ReverseOutcome::Found(pos) => assert_eq!(pos, pos_now - 100),
            other => panic!("reverse step did not land in history: {other:?}"),
        }
        assert_eq!(emu.retired_instructions(), pos_now - 100);
        match emu.tt_restore_to(pos_now)? {
            ReverseOutcome::Found(()) => {}
            other => panic!("restore_to forward replay failed: {other:?}"),
        }
        assert_eq!(
            sample(&emu),
            here,
            "replay to the original position diverged"
        );

        // The last instruction to write $2000 before now is the addq at $1000.
        match emu.tt_last_writer(0x2000, pos_now)? {
            ReverseOutcome::Found(rec) => {
                assert_eq!(rec.addr, 0x2000);
                assert_eq!(rec.pc, 0x1000, "addq is the only writer of $2000");
                assert_eq!(rec.new, rec.old.wrapping_add(1), "addq increments by one");
                assert_eq!(
                    emu.retired_instructions(),
                    rec.pos,
                    "machine parked on the writing instruction"
                );
            }
            other => panic!("last writer not found in history: {other:?}"),
        }
        Ok(())
    }

    /// Reverse replay re-applies logged input at its original position: a
    /// mouse motion noted mid-run is reproduced when replaying through that
    /// point from an earlier snapshot, so the reconstructed state matches.
    #[test]
    fn reverse_replay_reproduces_logged_input() -> Result<()> {
        use crate::config::PacingBudget;
        use crate::emulator::Emulator;
        use crate::inputsched::ReplayAction;
        use crate::timetravel::ReverseOutcome;

        let mut bus = test_bus_with_pc(0x1000);
        write_program(
            &mut bus,
            0x1000,
            &[
                0x5279, 0x0000, 0x2000, // addq.w  #1,$2000.l
                0x60FA, // bra.s   $1000
            ],
        );
        let mut emu = Emulator::new(
            bus,
            CpuModel::M68000,
            false,
            Default::default(),
            PacingBudget::Instructions,
            2,
            false,
        )?;
        // A huge interval keeps a single early anchor, so every reverse op
        // replays from before the input below -- exercising re-application.
        emu.enable_time_travel(256, 100_000);

        while emu.bus().emulated_frames() < 3 {
            emu.step_frame()?;
        }
        // Apply a port-1 mouse motion live and note it for replay, exactly as
        // the window's add_mouse_delta_i32 does.
        emu.bus_mut().input.add_mouse_delta(0, 40, 0);
        emu.tt_note_input(ReplayAction::MouseMove {
            port: 0,
            dx: 40,
            dy: 0,
        });

        while emu.bus().emulated_frames() < 6 {
            emu.step_frame()?;
        }
        let pos_now = emu.retired_instructions();
        let counter_with_input = emu.bus().input.ports[0].counter_x;
        assert_eq!(counter_with_input, 40, "the motion landed");

        // Reverse to before the motion: replaying from the early anchor stops
        // short of the note, so the counter is back to zero.
        match emu.tt_reverse_step(pos_now / 2)? {
            ReverseOutcome::Found(_) => {}
            other => panic!("reverse step failed: {other:?}"),
        }
        assert!(
            emu.retired_instructions() < pos_now,
            "actually stepped back"
        );

        // Replay forward through the note again: the motion is re-applied, so
        // the reconstructed counter matches the original timeline.
        match emu.tt_restore_to(pos_now)? {
            ReverseOutcome::Found(()) => {}
            other => panic!("restore_to failed: {other:?}"),
        }
        assert_eq!(
            emu.bus().input.ports[0].counter_x,
            counter_with_input,
            "replay did not reproduce the logged mouse motion"
        );
        Ok(())
    }

    /// A fired reverse watchpoint must not perturb the forward timeline: the
    /// state-mutating backward query is bracketed by snapshot/restore, so a
    /// run with the watch armed matches one without it instruction-for-byte.
    #[test]
    fn reverse_watchpoint_does_not_disturb_the_forward_run() -> Result<()> {
        use crate::config::PacingBudget;
        use crate::emulator::Emulator;

        let program = [
            0x5279, 0x0000, 0x2000, // addq.w  #1,$2000.l
            0x60FA, // bra.s   $1000
        ];
        let build = || -> Result<Emulator> {
            let mut bus = test_bus_with_pc(0x1000);
            write_program(&mut bus, 0x1000, &program);
            let mut emu = Emulator::new(
                bus,
                CpuModel::M68000,
                false,
                Default::default(),
                PacingBudget::Instructions,
                2,
                false,
            )?;
            emu.enable_time_travel(256, 2);
            Ok(emu)
        };

        let mut armed = build()?;
        // Fire the query partway through the run.
        armed.arm_reverse_watch(0x2000, Some(0.02));
        let mut plain = build()?;

        for _ in 0..8 {
            armed.step_frame()?;
            plain.step_frame()?;
        }

        assert_eq!(armed.machine.pc(), plain.machine.pc());
        assert_eq!(armed.machine.sr(), plain.machine.sr());
        assert_eq!(armed.bus().emulated_frames(), plain.bus().emulated_frames());
        assert_eq!(armed.bus().emulated_cck(), plain.bus().emulated_cck());
        assert_eq!(
            armed.bus().peek_word_any(0x2000),
            plain.bus().peek_word_any(0x2000),
            "the reverse watchpoint corrupted the forward timeline"
        );
        assert_eq!(armed.retired_instructions(), plain.retired_instructions());
        Ok(())
    }

    // ----- FPU (68881/68882 via the coprocessor interface) ---------------

    fn fpu_machine(bus: Bus) -> Result<M68kMachine> {
        M68kMachine::new(bus, CpuModel::M68020, true)
    }

    fn read_chip_f64(bus: &Bus, addr: u32) -> f64 {
        let hi = read_chip_long(bus, addr) as u64;
        let lo = read_chip_long(bus, addr + 4) as u64;
        f64::from_bits((hi << 32) | lo)
    }

    #[test]
    fn fpu_fmovecr_pi_round_trips_through_memory() -> Result<()> {
        let mut bus = test_bus_with_pc(0x0000_0100);
        write_program(
            &mut bus,
            0x0000_0100,
            &[
                0x207C, 0x0000, 0x3000, // MOVEA.L #$3000,A0
                0xF200, 0x5C00, // FMOVECR #0,FP0 (pi)
                0xF210, 0x7400, // FMOVE.D FP0,(A0)
            ],
        );
        let mut machine = fpu_machine(bus)?;
        machine.step_slice(3)?;
        assert_eq!(read_chip_f64(machine.bus(), 0x3000), std::f64::consts::PI);
        Ok(())
    }

    #[test]
    fn fpu_arithmetic_chain_produces_exact_double() -> Result<()> {
        // (3.0 + 14) / 4 = 4.25, exactly representable.
        let mut bus = test_bus_with_pc(0x0000_0100);
        write_program(
            &mut bus,
            0x0000_0100,
            &[
                0x207C, 0x0000, 0x3000, // MOVEA.L #$3000,A0
                0xF23C, 0x5400, 0x4008, 0x0000, 0x0000, 0x0000, // FMOVE.D #3.0,FP0
                0xF23C, 0x4022, 0x0000, 0x000E, // FADD.L #14,FP0
                0xF23C, 0x4020, 0x0000, 0x0004, // FDIV.L #4,FP0
                0xF210, 0x7400, // FMOVE.D FP0,(A0)
            ],
        );
        let mut machine = fpu_machine(bus)?;
        machine.step_slice(5)?;
        assert_eq!(read_chip_f64(machine.bus(), 0x3000), 4.25);
        Ok(())
    }

    #[test]
    fn fpu_fcmp_drives_fbcc_branches() -> Result<()> {
        // FP0 = 2, FP1 = 3; FCMP FP1,FP0 computes FP0 - FP1 = -1 (N set).
        // FBLT takes its branch; the fallthrough path would write a
        // different marker.
        let mut bus = test_bus_with_pc(0x0000_0100);
        write_program(
            &mut bus,
            0x0000_0100,
            &[
                0xF23C, 0x4000, 0x0000, 0x0002, // FMOVE.L #2,FP0
                0xF23C, 0x4080, 0x0000, 0x0003, // FMOVE.L #3,FP1
                0xF200, 0x0438, // FCMP.X FP1,FP0
                0xF294, 0x0008, // FBLT.W +8 (to 0x011A)
                0x31FC, 0x00BB, 0x3000, // MOVE.W #$BB,$3000.W (skipped)
                0x31FC, 0x00AA, 0x3000, // 0x011A: MOVE.W #$AA,$3000.W
            ],
        );
        let mut machine = fpu_machine(bus)?;
        machine.step_slice(5)?;
        assert_eq!(read_chip_word(machine.bus(), 0x3000), 0x00AA);
        Ok(())
    }

    #[test]
    fn fpu_fmove_extended_spill_and_reload_round_trips() -> Result<()> {
        // The compiler register-spill idiom: FMOVE.X FPn,-(A7) and back.
        let mut bus = test_bus_with_pc(0x0000_0100);
        write_program(
            &mut bus,
            0x0000_0100,
            &[
                0x207C, 0x0000, 0x3000, // MOVEA.L #$3000,A0
                0xF23C, 0x5400, 0x3FF8, 0xD3C2, 0x1234, 0x5678, // FMOVE.D #x,FP0
                0xF227, 0x6800, // FMOVE.X FP0,-(A7)
                0xF21F, 0x4980, // FMOVE.X (A7)+,FP3
                0xF210, 0x7580, // FMOVE.D FP3,(A0)
            ],
        );
        let mut machine = fpu_machine(bus)?;
        machine.step_slice(5)?;
        let expected = f64::from_bits(0x3FF8_D3C2_1234_5678);
        assert_eq!(read_chip_f64(machine.bus(), 0x3000), expected);
        Ok(())
    }

    #[test]
    fn fpu_fmovem_saves_and_restores_registers() -> Result<()> {
        let mut bus = test_bus_with_pc(0x0000_0100);
        write_program(
            &mut bus,
            0x0000_0100,
            &[
                0x207C, 0x0000, 0x3000, // MOVEA.L #$3000,A0
                0xF23C, 0x4000, 0x0000, 0x0007, // FMOVE.L #7,FP0
                0xF23C, 0x4080, 0x0000, 0x002A, // FMOVE.L #42,FP1
                0xF227, 0xE003, // FMOVEM.X FP0-FP1,-(A7) (predec mask: FPn in bit n)
                0xF23C, 0x4000, 0x0000, 0x0000, // FMOVE.L #0,FP0 (clobber)
                0xF23C, 0x4080, 0x0000, 0x0000, // FMOVE.L #0,FP1 (clobber)
                0xF21F, 0xD0C0, // FMOVEM.X (A7)+,FP0-FP1
                0xF210, 0x7400, // FMOVE.D FP0,(A0)
                0xF210, 0x7480, // FMOVE.D FP1,(A0) -- second run overwrites
            ],
        );
        let mut machine = fpu_machine(bus)?;
        machine.step_slice(8)?;
        assert_eq!(read_chip_f64(machine.bus(), 0x3000), 7.0);
        machine.step_slice(1)?;
        assert_eq!(read_chip_f64(machine.bus(), 0x3000), 42.0);
        Ok(())
    }

    #[test]
    fn fpu_indexed_addressing_reads_operands() -> Result<()> {
        // FMOVE.S (8,A0,D1.L),FP0 exercises the brief-extension-word
        // indexed mode through the FPU operand resolver.
        let mut bus = test_bus_with_pc(0x0000_0100);
        write_chip_long(&mut bus, 0x3000 + 8 + 4, (1.5f32).to_bits());
        write_program(
            &mut bus,
            0x0000_0100,
            &[
                0x207C, 0x0000, 0x3000, // MOVEA.L #$3000,A0
                0x223C, 0x0000, 0x0004, // MOVE.L #4,D1
                0xF230, 0x4400, 0x1808, // FMOVE.S (8,A0,D1.L),FP0
                0xF210, 0x7400, // FMOVE.D FP0,(A0)
            ],
        );
        let mut machine = fpu_machine(bus)?;
        machine.step_slice(4)?;
        assert_eq!(read_chip_f64(machine.bus(), 0x3000), 1.5);
        Ok(())
    }

    #[test]
    fn fpu_fmove_long_saturates_and_rounds() -> Result<()> {
        let mut bus = test_bus_with_pc(0x0000_0100);
        write_program(
            &mut bus,
            0x0000_0100,
            &[
                0x207C, 0x0000, 0x3000, // MOVEA.L #$3000,A0
                0xF23C, 0x5400, 0x4002, 0x6666, 0x6666, 0x6666, // FMOVE.D #2.3,FP0
                0xF210, 0x6000, // FMOVE.L FP0,(A0)
            ],
        );
        let mut machine = fpu_machine(bus)?;
        machine.step_slice(3)?;
        assert_eq!(read_chip_long(machine.bus(), 0x3000), 2);
        Ok(())
    }

    #[test]
    fn fpu_fdbcc_loops_until_count_expires() -> Result<()> {
        // FP condition F (never true) turns FDBF into a plain counted
        // loop: D1 counts 3,2,1,0,-1 and the loop body runs 4 times.
        let mut bus = test_bus_with_pc(0x0000_0100);
        write_program(
            &mut bus,
            0x0000_0100,
            &[
                0x223C, 0x0000, 0x0003, // MOVE.L #3,D1
                0x4242, // CLR.W D2
                0x5242, // 0x010A: ADDQ.W #1,D2
                0xF249, 0x0000, 0xFFFA, // FDBF D1,-6 (back to 0x010A)
                0x31C2, 0x3000, // MOVE.W D2,$3000.W
            ],
        );
        let mut machine = fpu_machine(bus)?;
        machine.step_slice(2 + 4 * 2 + 1)?;
        assert_eq!(read_chip_word(machine.bus(), 0x3000), 4);
        Ok(())
    }

    #[test]
    fn fpu_fsave_frestore_round_trip_preserves_null_state() -> Result<()> {
        // The exec-style detection/context-switch sequence: FSAVE writes
        // a frame, FRESTORE consumes it. After an FRESTORE of a NULL
        // frame the next FSAVE writes a NULL frame (one long).
        let mut bus = test_bus_with_pc(0x0000_0100);
        write_program(
            &mut bus,
            0x0000_0100,
            &[
                0x2E7C, 0x0000, 0x4000, // MOVEA.L #$4000,A7
                0xF327, // FSAVE -(A7)
                0xF35F, // FRESTORE (A7)+
                0x31FC, 0x0001, 0x3000, // MOVE.W #1,$3000.W (alive marker)
            ],
        );
        let mut machine = fpu_machine(bus)?;
        machine.step_slice(4)?;
        assert_eq!(read_chip_word(machine.bus(), 0x3000), 1);
        // The stack pointer is balanced after the save/restore pair.
        assert_eq!(machine.cpu.a(7), 0x4000);
        Ok(())
    }

    #[test]
    fn fpu_disabled_machine_still_traps_f_line() -> Result<()> {
        // Without an FPU fitted the same instruction takes the Line-F
        // exception, which is how boot code detects the FPU's absence.
        let mut bus = test_bus_with_pc(0x0000_0100);
        write_program(&mut bus, 0x0000_0100, &[0xF200, 0x5C00]); // FMOVECR #0,FP0
        write_chip_long(&mut bus, 0x2C, 0x0000_0200); // Line-F vector
        write_program(&mut bus, 0x0000_0200, &[0x4E71]); // handler NOP
        let mut machine = M68kMachine::new(bus, CpuModel::M68020, false)?;
        machine.step_slice(1)?;
        assert_eq!(machine.pc(), 0x0000_0200);
        Ok(())
    }

    #[test]
    fn watch_words_odd_start_even_length() {
        let watch = crate::debugger::Watch {
            addr: 0x1001,
            len: 2,
        };
        let base = watch.addr & !1;
        let count = ((watch.addr & 1) + watch.len).div_ceil(2);
        let words: Vec<u32> = (0..count).map(|k| base + k * 2).collect();
        assert_eq!(words, vec![0x1000, 0x1002]);
    }

    /// The uaelib trap stub (crate::uaelib) at $F0FF60: served from its
    /// fixed 32-byte region on every CPU, floating outside it, hidden by a
    /// CDTV extended ROM, and kept out of both caches.
    #[test]
    fn uaelib_stub_is_served_from_the_fixed_region_and_floats_outside_it() -> Result<()> {
        let mut bus = test_bus_with_pc(0x00F8_0100);
        bus.attach_uaelib(crate::uaelib::UaeLib::new());
        let mut machine = build(bus, CpuModel::M68000, false, 2, Default::default(), false)?;
        assert_eq!(machine.bus.read_word(0x00F0_FF60), 0x4EB9);
        assert_eq!(machine.bus.read_long(0x00F0_FF60), 0x4EB9_00F0);
        assert_eq!(machine.bus.read_word(0x00F0_FF74), 0x4E75);
        // Outside the region the bus floats to the last driven value.
        let floating = machine.bus.read_word(0x00F0_FF80);
        assert_eq!(floating, machine.bus.bus.data_bus);
        let floating = machine.bus.read_word(0x00F0_0000);
        assert_eq!(floating, machine.bus.bus.data_bus);
        // The debugger's side-effect-free peeks see the stub too.
        assert_eq!(machine.bus.bus.peek_word_any(0x00F0_FF60), 0x4EB9);
        assert_eq!(machine.bus.peek_byte(0x00F0_FF61), 0xB9);
        assert_eq!(machine.bus.peek_byte(0x00F0_FF80), 0xFF);
        assert!(!machine.bus.icache_cacheable(0x00F0_FF60));
        assert!(!machine.bus.dcache_cacheable(0x00F0_FF60));

        // A CDTV extended ROM at $F00000 decodes first and hides the trap.
        machine
            .bus
            .bus
            .mem
            .attach_extended_rom(vec![0x11; 0x4_0000])?;
        assert_eq!(machine.bus.read_word(0x00F0_FF60), 0x1111);
        // A CD32's lives at $E00000 and does not.
        machine
            .bus
            .bus
            .mem
            .attach_extended_rom(vec![0x11; 0x8_0000])?;
        assert_eq!(machine.bus.read_word(0x00F0_FF60), 0x4EB9);

        // Without the trap fitted the region floats like any other.
        let bus = test_bus_with_pc(0x00F8_0100);
        let mut machine = build(bus, CpuModel::M68000, false, 2, Default::default(), false)?;
        let floating = machine.bus.read_word(0x00F0_FF60);
        assert_eq!(floating, machine.bus.bus.data_bus);
        Ok(())
    }

    /// Bartman's `KPrintF` shape: `UaeDbgLog(86, string)` as a C call.
    fn uaelib_log_program() -> Vec<u16> {
        vec![
            0x4879, 0x0000, 0x2000, // pea ($2000).l      ; string
            0x4878, 0x0056, // pea (86).w         ; function
            0x4EB9, 0x00F0, 0xFF60, // jsr ($F0FF60).l
            0x508F, // addq.l #8,a7
            0x23C0, 0x0000, 0x1000, // move.l d0,($1000).l
            0x60FE, // bra.s *
        ]
    }

    /// Bartman's `warpmode()` shape: `UaeConf(82, -1, "warp true", 0, &out, 1)`.
    fn uaelib_warp_program(out: u32) -> Vec<u16> {
        vec![
            0x4878,
            0x0001, // pea (1).w          ; outsize
            0x4879,
            (out >> 16) as u16,
            out as u16, // pea (out).l
            0x4878,
            0x0000, // pea (0).w          ; size: measure
            0x4879,
            0x0000,
            0x2000, // pea ($2000).l      ; "warp true"
            0x4878,
            0xFFFF, // pea (-1).w         ; index -1
            0x4878,
            0x0052, // pea (82).w
            0x4EB9,
            0x00F0,
            0xFF60, // jsr ($F0FF60).l
            0x4FEF,
            0x0018, // lea 24(a7),a7
            0x23C0,
            0x0000,
            0x1000, // move.l d0,($1000).l
            0x60FE, // bra.s *
        ]
    }

    fn write_chip_bytes(bus: &mut Bus, address: u32, bytes: &[u8]) {
        let off = region_offset(bus.mem.chip_ram.len(), CHIP_RAM_BASE, address, bytes.len())
            .expect("test chip address must fit chip RAM");
        bus.mem.chip_ram[off..off + bytes.len()].copy_from_slice(bytes);
    }

    /// A 68000 rings the doorbell as two word writes, a 68030 as one long
    /// write; both reach the arguments on the guest stack through the
    /// stub and get D0 back.
    #[test]
    fn uaelib_call_through_the_stub_reads_its_arguments_from_the_guest_stack() -> Result<()> {
        for model in [CpuModel::M68000, CpuModel::M68030] {
            let mut bus = test_bus_with_pc(0x00F8_0100);
            bus.attach_uaelib(crate::uaelib::UaeLib::new());
            write_chip_bytes(&mut bus, 0x2000, b"hello\0");
            write_program(&mut bus, 0x00F8_0100, &uaelib_log_program());
            let mut machine = build(bus, model, false, 2, Default::default(), false)?;
            machine.step_slice(11)?;
            let mem = &machine.bus.bus.mem;
            assert_eq!(
                &mem.chip_ram[0x1000..0x1004],
                &1u32.to_be_bytes(),
                "{model:?}: function 86 returns 1 in D0"
            );
            let uaelib = machine.bus.bus.uaelib.as_mut().unwrap();
            assert_eq!(
                uaelib.take_debug_events(),
                (vec![crate::uaelib::DebugEvent::Log("hello".into())], 0),
                "{model:?}"
            );

            let mut bus = test_bus_with_pc(0x00F8_0100);
            bus.attach_uaelib(crate::uaelib::UaeLib::new());
            write_chip_bytes(&mut bus, 0x2000, b"warp true\0");
            write_chip_bytes(&mut bus, 0x3000, &[0xAA]);
            write_chip_bytes(&mut bus, 0x1000, &[0xEE; 4]);
            write_program(&mut bus, 0x00F8_0100, &uaelib_warp_program(0x3000));
            let mut machine = build(bus, model, false, 2, Default::default(), false)?;
            machine.step_slice(15)?;
            let mem = &machine.bus.bus.mem;
            assert_eq!(
                &mem.chip_ram[0x1000..0x1004],
                &[0; 4],
                "{model:?}: returns 0"
            );
            assert_eq!(mem.chip_ram[0x3000], 0, "{model:?}: out string cleared");
            let uaelib = machine.bus.bus.uaelib.as_mut().unwrap();
            assert_eq!(uaelib.take_warp_request(), Some(true), "{model:?}");
        }
        Ok(())
    }

    /// A lone high-word doorbell write (the first half of a 68000 move.l)
    /// is visible through the peeks and has not fired.
    #[test]
    fn uaelib_half_written_doorbell_is_latched_but_silent() -> Result<()> {
        let mut bus = test_bus_with_pc(0x00F8_0100);
        bus.attach_uaelib(crate::uaelib::UaeLib::new());
        let mut machine = build(bus, CpuModel::M68000, false, 2, Default::default(), false)?;
        machine.bus.write_word(0x00F0_FF7C, 0x0001);
        assert_eq!(machine.bus.bus.peek_word_any(0x00F0_FF7C), 0x0001);
        assert_eq!(machine.bus.read_long(0x00F0_FF78), 0, "no result latched");
        let uaelib = machine.bus.bus.uaelib.as_mut().unwrap();
        assert!(uaelib.take_debug_events().0.is_empty());
        Ok(())
    }

    /// Function 82 clears the caller's out string behind a 68030's data
    /// cache: the completed doorbell drops the cache so the guest reads the
    /// NUL back, not the stale line.
    #[test]
    fn uaelib_doorbell_completion_drops_a_stale_data_cache_line() -> Result<()> {
        let out = FAST_RAM_BASE as u32 + 0x40;
        let program = [
            vec![
                0x203C,
                0x0000,
                0x0101, // move.l #$101,d0    ; CACR: EI + ED
                0x4E7B,
                0x0002, // movec d0,cacr
                0x2239,
                (out >> 16) as u16,
                out as u16, // move.l (out).l,d1 ; fill the line
            ],
            uaelib_warp_program(out),
        ]
        .concat();
        // The warp program ends in bra.s *; patch a read-back before it.
        let mut program = program;
        let bra = program.len() - 1;
        program.splice(
            bra..bra,
            [
                0x2439,
                (out >> 16) as u16,
                out as u16, // move.l (out).l,d2
                0x23C2,
                0x0000,
                0x1004, // move.l d2,($1004).l
            ],
        );
        let mut bus = test_bus_with_pc(0x00F8_0100);
        bus.attach_uaelib(crate::uaelib::UaeLib::new());
        write_chip_bytes(&mut bus, 0x2000, b"warp true\0");
        write_program(&mut bus, 0x00F8_0100, &program);
        let off = bus.mem.zorro.region_at(out, 4).unwrap();
        bus.mem.zorro.board_ram_mut(off.0)[off.1..off.1 + 4].copy_from_slice(&[0xAA; 4]);
        let mut machine = build(bus, CpuModel::M68030, false, 2, Default::default(), false)?;
        machine.set_cache_emulation(true, true);
        machine.step_slice(20)?;
        let mem = &machine.bus.bus.mem;
        assert_eq!(
            &mem.chip_ram[0x1004..0x1008],
            &0x00AA_AAAAu32.to_be_bytes(),
            "the NUL the host wrote is what the guest reads back"
        );
        Ok(())
    }
}
