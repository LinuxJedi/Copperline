// SPDX-License-Identifier: GPL-3.0-or-later

//! M68K CPU wrapper and CPU-visible Amiga bus adapter.

use crate::bus::{Bus, CpuBusAccessKind};
use crate::chipset::cia::CiaSideEffect;
use crate::chipset::paula::{pending_ipl, INT_MASTER};
use crate::config::CpuModel;
use crate::memory::{AUTOCONFIG_BASE, AUTOCONFIG_SIZE, CHIP_RAM_BASE, ROM_BASE, SLOW_RAM_BASE};
use anyhow::{anyhow, Result};
use log::{debug, trace};
use m68k::{AddressBus, CpuCore, CpuType, NoOpHleHandler, StepResult};

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

pub struct M68kMachine {
    cpu: CpuCore,
    bus: CpuBus,
    hle: NoOpHleHandler,
    fpu_enabled: bool,
    /// Last CACR value pushed into the cache models, so the per-instruction
    /// sync is a single compare when nothing changed.
    last_cacr: u32,
    dbg_pc_hist: std::collections::HashMap<u32, u64>,
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
    dbg_sched_bhist: std::collections::HashMap<u32, u64>,
    dbg_sched_bhist_dumped: bool,
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
    bincode::deserialize_from(r).map_err(|e| anyhow!("reading {what}: {e}"))
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
}

pub fn build(
    bus: Bus,
    cpu_model: CpuModel,
    fpu_enabled: bool,
    cpu_clocks_per_cck: u32,
    _track_stopped_slice_progress: bool,
) -> Result<M68kMachine> {
    let mut machine = M68kMachine::new(bus, cpu_model, fpu_enabled)?;
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
            },
            hle: NoOpHleHandler,
            fpu_enabled,
            last_cacr: 0,
            dbg_ipl_main_cyc: 0,
            dbg_ipl_irq_cyc: 0,
            dbg_ipl_last_frame: 0,
            dbg_pc_hist: std::collections::HashMap::new(),
            dbg_pc_cyc: std::collections::HashMap::new(),
            dbg_in_window: false,
            dbg_pc_on: crate::envcfg::flag("COPPERLINE_DIAG_PCHIST"),
            dbg_pc_dumped: false,
            dbg_crash_ring: std::collections::VecDeque::with_capacity(65),
            dbg_crash_on: crate::envcfg::flag("COPPERLINE_DIAG_CRASH"),
            dbg_crash_dumped: false,
            dbg: crate::debugger::Debugger::from_env(),
            ui_breaks: crate::debugger::InteractiveBreaks::default(),
            ui_stop: None,
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
            dbg_sched_bhist: std::collections::HashMap::new(),
            dbg_sched_bhist_dumped: false,
            dbg_fc_addr: crate::envcfg::var("COPPERLINE_DBG_FC")
                .and_then(|s| u32::from_str_radix(s.trim().trim_start_matches("0x"), 16).ok()),
            dbg_ipl_on: crate::envcfg::flag("COPPERLINE_DIAG_IPL"),
            dbg_spren_on: crate::envcfg::flag("COPPERLINE_DBG_SPREN"),
        };
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

    // COPPERLINE_DIAG_CRASH: keep a short history of (pc, opcode, A7) and, the first
    // time the CPU executes a sustained run of zero opcodes from chip RAM (real
    // empty memory -- a single 0x0000 opcode is a valid ORI.B #imm,D0), dump the
    // history, the stack and the registers. Catches "demo jumped into empty
    // memory" crashes with the path that led there.
    fn dbg_record_crash_path(&mut self) {
        if self.dbg_crash_dumped {
            return;
        }
        let pc = self.cpu.pc;
        let opcode = if (pc as usize) < self.bus.bus.mem.chip_ram.len() {
            self.bus.bus.peek_chip_word(pc as usize)
        } else {
            0xFFFF
        };
        // Require a run of zero words at and after the PC: 8 consecutive zero
        // words cannot be real code (ORI.B #0,D0 spam), only empty memory.
        let in_empty_memory =
            opcode == 0 && (1..8).all(|k| self.bus.bus.peek_chip_word(pc as usize + k * 2) == 0);
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
            log::warn!(
                "CRASH: CPU entered zero-opcode memory at PC={:#010X} t={:.4}s SR={:#06X}",
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
            for reg in 0..8 {
                log::warn!(
                    "  D{reg}={:#010X} A{reg}={:#010X}",
                    self.cpu.d(reg),
                    self.cpu.a(reg)
                );
            }
            let sp = self.cpu.a(7) as usize;
            let mut stack = String::new();
            for k in 0..24 {
                stack.push_str(&format!("{:04X} ", self.bus.bus.peek_chip_word(sp + k * 2)));
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
                        self.bus.bus.peek_chip_word(base as usize + k * 2)
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
    /// post-step pass can detect writes. Returns `None` when inactive or when
    /// there is nothing to compare afterwards.
    fn debug_before_step(&mut self) -> Option<Vec<(u32, u16)>> {
        let secs = self.bus.bus.emulated_seconds();
        if !self.dbg.as_ref().is_some_and(|d| d.enabled_at(secs)) {
            return None;
        }
        self.maybe_dump_copper(secs);
        self.maybe_dump_ram(secs);
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

        if self.dbg.as_ref().is_some_and(|d| d.is_breakpoint(pc)) {
            self.debug_report(&format!("BREAK pc={pc:#010X}"), secs);
        }

        let watch_words: Vec<u32> = self
            .dbg
            .as_ref()
            .map(|d| {
                d.watches
                    .iter()
                    .flat_map(|w| (0..w.len.div_ceil(2)).map(move |k| w.addr + k * 2))
                    .collect()
            })
            .unwrap_or_default();
        if watch_words.is_empty() {
            return None;
        }
        Some(
            watch_words
                .into_iter()
                .map(|addr| (addr, self.bus.bus.peek_word_any(addr)))
                .collect(),
        )
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
        // Fast path: when not actively capturing the task-B span, only the five
        // breakpoint PCs matter.
        if !self.dbg_sched_capturing && !is_bp {
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
        // pc == NUDGE: report the iteration span and its render / task-B split.
        let frame_delta = frame.wrapping_sub(self.dbg_sched_top_frame);
        let counter_delta = counter.wrapping_sub(self.dbg_sched_top_counter);
        log::info!(
            "sched iter secs={secs:.5} top(f={} v={}) render(f={} v={}) b8(f={} v={}) \
             nudge(f={} v={}) frame_delta={} counter_delta={} crossed_vblank={} d2={:#X}",
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
        let mut fb = vec![0u32; crate::video::MAX_FB_PIXELS];
        crate::video::bitplane::render(&mut self.bus.bus, &mut fb);
        let geometry = self.bus.bus.frame_geometry();
        if !geometry.programmable {
            let visible_start = self.bus.bus.frame_visible_start_vpos();
            crate::video::window::center_present_frame_for_visible_start(&mut fb, visible_start);
        } else if geometry.line_cck != 227 {
            crate::screenshot::stretch_rows_x(
                &mut fb,
                crate::video::FB_WIDTH,
                geometry.visible_lines,
                geometry.line_cck,
                227,
            );
        }
        match crate::screenshot::save_scaled_y(
            std::path::Path::new(&path),
            &fb,
            crate::video::FB_WIDTH as u32,
            geometry.visible_lines as u32,
            crate::video::PRESENT_HEIGHT as u32,
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

    /// Serialize the machine's emulated state (CPU core, timing carries,
    /// cache models, Bus) into `w`, in the fixed component order
    /// `apply_state` reads back. Host-side state (debugger instrumentation,
    /// sinks, trace files) is not written. Call only at an emulated-frame
    /// boundary; mid-frame the renderer capture buffers are inconsistent.
    pub(crate) fn write_state<W: std::io::Write>(&self, w: &mut W) -> Result<()> {
        serialize_component(w, &self.cpu, "CPU core")?;
        let runtime = MachineRuntimeState {
            last_cacr: self.last_cacr,
            sync_cck_on: self.sync_cck_on,
            cpu_clocks_per_cck: self.cpu_clocks_per_cck,
            cpu_clock_carry: self.cpu_clock_carry,
        };
        serialize_component(w, &runtime, "machine runtime")?;
        serialize_component(w, &self.bus.icache, "icache")?;
        serialize_component(w, &self.bus.dcache, "dcache")?;
        serialize_component(w, &self.bus.bus, "bus")?;
        Ok(())
    }

    /// Counterpart of `write_state`: parse every component from `r`, then
    /// swap the machine onto the restored state. The live machine is left
    /// untouched if any component fails to parse. Host resources (audio and
    /// serial sinks, blitter trace file) move across to the restored Bus;
    /// debugger state and breakpoints stay live.
    pub(crate) fn apply_state<R: std::io::Read>(&mut self, r: &mut R) -> Result<()> {
        let cpu: CpuCore = deserialize_component(r, "CPU core")?;
        let runtime: MachineRuntimeState = deserialize_component(r, "machine runtime")?;
        let icache: Option<Box<crate::cache::CpuCache>> = deserialize_component(r, "icache")?;
        let dcache: Option<Box<crate::cache::CpuCache>> = deserialize_component(r, "dcache")?;
        let mut bus: Bus = deserialize_component(r, "bus")?;

        bus.adopt_host_resources(&mut self.bus.bus);
        // The CPU model travels with the state (cpu_type, timing tables, and
        // address_mask all live in CpuCore); keep the bus adapter's mask copy
        // in step so external decode agrees with the restored core.
        self.bus.address_mask = cpu.address_mask;
        self.cpu = cpu;
        self.bus.bus = bus;
        self.bus.icache = icache;
        self.bus.dcache = dcache;
        self.last_cacr = runtime.last_cacr;
        self.sync_cck_on = runtime.sync_cck_on;
        self.cpu_clocks_per_cck = runtime.cpu_clocks_per_cck;
        self.cpu_clock_carry = runtime.cpu_clock_carry;
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

    pub fn a(&self, reg: usize) -> u32 {
        self.cpu.a(reg)
    }

    pub fn d(&self, reg: usize) -> u32 {
        self.cpu.d(reg)
    }

    pub fn cpu_type(&self) -> CpuType {
        self.cpu.cpu_type
    }

    /// Whether the CPU is halted in STOP waiting for an interrupt.
    pub fn stopped(&self) -> bool {
        self.cpu.stopped != 0
    }

    /// The debugger window's breakpoint/watchpoint set (read-only view).
    pub fn ui_breaks(&self) -> &crate::debugger::InteractiveBreaks {
        &self.ui_breaks
    }

    /// Toggle a PC breakpoint. Returns true when the breakpoint is now set.
    pub fn ui_toggle_breakpoint(&mut self, addr: u32) -> bool {
        self.ui_breaks.toggle_breakpoint(addr)
    }

    /// Toggle a word watchpoint at `addr` (word-aligned), baselining it on
    /// the current memory contents. Returns true when now set.
    pub fn ui_toggle_watch(&mut self, addr: u32) -> bool {
        let addr = addr & crate::debugger::UI_ADDR_MASK & !1;
        let current = self.bus.bus.peek_word_any(addr);
        self.ui_breaks.toggle_watch(addr, current)
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

    pub fn ui_breaks_clear(&mut self) {
        self.ui_breaks.clear();
        self.bus.bus.set_ui_reg_watches(&[]);
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

    /// Promote a custom-register watch hit recorded by the bus write
    /// path into the machine-level stop reason.
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
        let pc = self.cpu.pc & crate::debugger::UI_ADDR_MASK;
        if self.ui_breaks.is_breakpoint(pc) {
            self.ui_stop = Some(DebugStop::Breakpoint { pc });
            return;
        }
        self.ui_promote_reg_hit();
        if self.ui_stop.is_some() {
            return;
        }
        let writer_pc = self.cpu.ppc & crate::debugger::UI_ADDR_MASK;
        for watch in &mut self.ui_breaks.watches {
            let new = self.bus.bus.peek_word_any(watch.addr);
            if new != watch.last {
                let old = watch.last;
                watch.last = new;
                self.ui_stop = Some(DebugStop::Watch {
                    addr: watch.addr,
                    old,
                    new,
                    writer_pc,
                });
                return;
            }
        }
    }

    pub fn reset_after_bus_reset(&mut self) {
        self.reset_cpu();
    }

    pub fn refresh_irq_line(&mut self) {
        // Apply any timed-device color clocks the last instruction's bus
        // accesses deferred before sampling the interrupt line. This runs at the
        // top of `service_pending_irq_cycles`, i.e. before every instruction, so
        // a device interrupt that came due during the previous instruction is
        // recognized at the correct boundary even within a multi-instruction
        // core slice.
        self.bus.bus.flush_timed_devices();
        let level = self.pending_irq_level();
        self.cpu.set_irq(level);
    }

    pub fn disable_overlay(&mut self) {
        self.bus.bus.mem.overlay = false;
        self.bus.bus.overlay_disable_pending = false;
        debug!("overlay disabled (chip RAM mapped at $0)");
    }

    pub fn step_slice(&mut self, count: usize) -> Result<CpuStepSlice> {
        self.bus.bus.begin_cpu_slice();
        self.bus.bus.slice_preempted = false;
        self.bus.bus.set_cpu_bus_arbitration_enabled(true);

        let mut instructions = 0usize;
        let mut cpu_cycles = 0u32;
        let mut cpu_cck = 0u32;
        let mut stopped = false;

        while instructions < count {
            let bus_before = self.bus.bus.slice_bus_advanced_cck();
            let cpu_cck_before = cpu_cck;
            if let Some(irq_cycles) = self.service_pending_irq_cycles() {
                cpu_cycles = cpu_cycles.saturating_add(positive_cpu_cycles(irq_cycles));
                cpu_cck = cpu_cck.saturating_add(self.charge_cpu_clocks(irq_cycles));
                // An interrupt dispatch can land the PC on a breakpoint;
                // stop before the handler's first instruction executes.
                if self.ui_breaks.armed() {
                    let pc = self.cpu.pc & crate::debugger::UI_ADDR_MASK;
                    if self.ui_breaks.is_breakpoint(pc) {
                        self.ui_stop = Some(crate::debugger::DebugStop::Breakpoint { pc });
                        break;
                    }
                }
            }
            if self.force_fpu_line_f_if_needed() {
                instructions = instructions.saturating_add(1);
                cpu_cycles = cpu_cycles.saturating_add(34);
                cpu_cck = cpu_cck.saturating_add(self.charge_cpu_clocks(34));
            } else {
                let dbg_pc_before = self.cpu.pc;
                let dbg_ipl_before = (self.cpu.get_sr() >> 8) & 7;
                if self.dbg_pc_on {
                    self.dbg_record_pc();
                }
                if self.dbg_crash_on {
                    self.dbg_record_crash_path();
                }
                if self.dbg_sched_on {
                    self.debug_check_sched(dbg_pc_before);
                }
                let dbg_watch_snapshot = if self.dbg.is_some() {
                    self.debug_before_step()
                } else {
                    None
                };
                match self.cpu.step_with_hle_handler(&mut self.bus, &mut self.hle) {
                    StepResult::Ok { cycles } => {
                        if self.bus.icache.is_some() || self.bus.dcache.is_some() {
                            self.apply_cacr_updates();
                        }
                        instructions = instructions.saturating_add(1);
                        cpu_cycles = cpu_cycles.saturating_add(positive_cpu_cycles(cycles));
                        cpu_cck = cpu_cck.saturating_add(self.charge_cpu_clocks(cycles));
                        if let Some(snapshot) = dbg_watch_snapshot {
                            self.debug_after_step(snapshot);
                        }
                        self.debug_check_spren_clear();
                        self.debug_check_frame_counter();
                        self.debug_check_memw();
                        self.debug_check_ipl(dbg_ipl_before, positive_cpu_cycles(cycles));
                        if self.ui_breaks.armed() {
                            self.ui_check_breaks_after_step();
                        }
                        if self.dbg_pc_on && self.dbg_in_window {
                            *self.dbg_pc_cyc.entry(dbg_pc_before).or_insert(0) +=
                                u64::from(positive_cpu_cycles(cycles));
                        }
                    }
                    StepResult::Stopped => {
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

            if self.sync_cck_on {
                // Advance the chipset through this instruction's CPU-internal
                // cycles (cpu_cck minus the cycles already spent on the bus) so
                // the beam and DMA track the CPU's full instruction time.
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

            // Stop the slice at an interactive breakpoint/watch hit; the
            // emulator ends the frame early and the window pauses.
            if self.ui_stop.is_some() {
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
        self.bus.bus.flush_timed_devices();
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
        let total = positive_cpu_cycles(cycles) + self.cpu_clock_carry;
        let cck = total / self.cpu_clocks_per_cck;
        self.cpu_clock_carry = total % self.cpu_clocks_per_cck;
        cck
    }

    /// Install the opt-in 68020/030 cache models. Until this is called (or
    /// with both false), CACR writes are tracked but have no effect, which
    /// is the calibrated default behaviour.
    pub fn set_cache_emulation(&mut self, icache: bool, dcache: bool) {
        self.bus.icache = icache.then(Box::default);
        self.bus.dcache = dcache.then(Box::default);
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
        use m68k::{CACR_CD, CACR_CED, CACR_CEI, CACR_CI, CACR_ED, CACR_EI, CACR_FD, CACR_FI};
        let ops = std::mem::take(&mut self.cpu.cacr_pending_ops);
        let cacr = self.cpu.cacr;
        if ops == 0 && cacr == self.last_cacr {
            return;
        }
        self.last_cacr = cacr;
        let caar = self.cpu.caar;
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

    fn service_pending_irq_cycles(&mut self) -> Option<i32> {
        self.refresh_irq_line();
        if !self.cpu.check_interrupts() {
            return None;
        }
        Some(self.cpu.execute(&mut self.bus, 0))
    }

    fn pending_irq_level(&self) -> u8 {
        let bus = &self.bus.bus;
        if bus.paula.intena & INT_MASTER == 0 {
            return 0;
        }
        let pending = bus.paula.intena & bus.cpu_visible_intreq();
        let level = pending_ipl(pending);
        if level == 0 {
            return 0;
        }

        let vector_addr = self
            .cpu
            .vbr
            .wrapping_add(0x60)
            .wrapping_add(u32::from(level) * 4);
        if self.bus.peek_long(vector_addr) == 0 {
            return 0;
        }
        level
    }

    fn force_fpu_line_f_if_needed(&mut self) -> bool {
        if self.fpu_enabled {
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

impl CpuBus {
    fn mask(&self, address: u32) -> u32 {
        address & self.address_mask
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
        if let Some(off) = self.overlay_rom_offset(addr, 1) {
            return self.bus.mem.rom[off];
        }
        if let Some(off) = region_offset(self.bus.mem.chip_ram.len(), CHIP_RAM_BASE, addr, 1) {
            return self.bus.mem.chip_ram[off];
        }
        if let Some((board, off)) = self.bus.mem.zorro.region_at(addr, 1) {
            return self.bus.mem.zorro.board_ram(board)[off];
        }
        if let Some(off) = region_offset(self.bus.mem.slow_ram.len(), SLOW_RAM_BASE, addr, 1) {
            return self.bus.mem.slow_ram[off];
        }
        if let Some(off) = region_offset(self.bus.mem.rom.len(), ROM_BASE, addr, 1) {
            return self.bus.mem.rom[off];
        }
        if let Some(off) = region_offset(
            self.bus.mem.extended_rom.len(),
            self.bus.mem.extended_rom_base,
            addr,
            1,
        ) {
            return self.bus.mem.extended_rom[off];
        }
        0xFF
    }

    /// Word-granular bus cycles for a CPU access of `size` bytes (the 68000
    /// performs one bus cycle per 16-bit word).
    fn access_words(size: usize) -> u32 {
        size.max(1).div_ceil(2) as u32
    }

    fn read_sized(&mut self, address: u32, size: usize, kind: CpuBusAccessKind) -> u32 {
        let addr = self.mask(address);
        if self.icache.is_some() || self.dcache.is_some() {
            // 68020/030 cache models: a hit costs no bus cycle at all; a
            // miss goes through the normal (billed) path and then fills
            // the line from backing memory.
            if let Some(value) = self.cache_read(addr, size, kind) {
                return value;
            }
            let value = self.read_sized_uncached(addr, size, kind);
            self.cache_fill_after_miss(addr, size, kind);
            return value;
        }
        self.read_sized_uncached(addr, size, kind)
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
        region_offset(self.bus.mem.chip_ram.len(), CHIP_RAM_BASE, addr, 1).is_some()
            || self.bus.mem.zorro.region_at(addr, 1).is_some()
            || region_offset(self.bus.mem.slow_ram.len(), SLOW_RAM_BASE, addr, 1).is_some()
            || region_offset(self.bus.mem.rom.len(), ROM_BASE, addr, 1).is_some()
            || region_offset(
                self.bus.mem.extended_rom.len(),
                self.bus.mem.extended_rom_base,
                addr,
                1,
            )
            .is_some()
    }

    /// What the data cache may hold: expansion RAM and ROM only. Chip and
    /// slow RAM are excluded (CIIN), as on real Amigas, because DMA writes
    /// them behind the CPU's back.
    fn dcache_cacheable(&self, addr: u32) -> bool {
        if self.overlay_rom_offset(addr, 1).is_some() {
            return false;
        }
        self.bus.mem.zorro.region_at(addr, 1).is_some()
            || region_offset(self.bus.mem.rom.len(), ROM_BASE, addr, 1).is_some()
            || region_offset(
                self.bus.mem.extended_rom.len(),
                self.bus.mem.extended_rom_base,
                addr,
                1,
            )
            .is_some()
    }

    fn read_sized_uncached(&mut self, address: u32, size: usize, kind: CpuBusAccessKind) -> u32 {
        let addr = self.mask(address);
        if let Some(off) = self.overlay_rom_offset(addr, size) {
            self.bus.cpu_external_access(Self::access_words(size));
            return read_be(&self.bus.mem.rom, off, size);
        }
        if let Some(off) = region_offset(self.bus.mem.chip_ram.len(), CHIP_RAM_BASE, addr, size) {
            self.bus.grant_cpu_bus_access_at(Some(addr), size, kind);
            return read_be(&self.bus.mem.chip_ram, off, size);
        }
        if let Some((board, off)) = self.bus.mem.zorro.region_at(addr, size) {
            // Expansion (Zorro) RAM runs at external-bus speed, off the
            // chip bus, exactly like the old fixed fast RAM mapping.
            self.bus.cpu_external_access(Self::access_words(size));
            return read_be(self.bus.mem.zorro.board_ram(board), off, size);
        }
        if let Some(off) = region_offset(self.bus.mem.slow_ram.len(), SLOW_RAM_BASE, addr, size) {
            // "Slow"/trapdoor RAM at $C00000 is decoded by Gary and reached
            // through Agnus on the chip bus, so the CPU contends for it cycle
            // by cycle exactly like chip RAM (that is why it is "slow" and does
            // not accelerate chip-bus-bound code). Arbitrate it like chip RAM,
            // not as uncontended external memory.
            self.bus.grant_cpu_bus_access_at(Some(addr), size, kind);
            return read_be(&self.bus.mem.slow_ram, off, size);
        }
        if let Some(off) = region_offset(self.bus.mem.rom.len(), ROM_BASE, addr, size) {
            self.bus.cpu_external_access(Self::access_words(size));
            return read_be(&self.bus.mem.rom, off, size);
        }
        if let Some(off) = region_offset(
            self.bus.mem.extended_rom.len(),
            self.bus.mem.extended_rom_base,
            addr,
            size,
        ) {
            self.bus.cpu_external_access(Self::access_words(size));
            return read_be(&self.bus.mem.extended_rom, off, size);
        }

        if range_contains(CIA_A_BASE, CIA_A_SIZE, addr) {
            self.bus.cpu_slow_external_access(Self::access_words(size));
            return self.bus.cia_a_read(u64::from(addr - CIA_A_BASE), size) as u32;
        }
        if range_contains(CIA_B_BASE, CIA_B_SIZE, addr) {
            self.bus.cpu_slow_external_access(Self::access_words(size));
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
        if self.bus.rtc_present() && range_contains(RTC_BASE, RTC_SIZE, addr) {
            self.bus.cpu_slow_external_access(Self::access_words(size));
            return self.bus.rtc.read(u64::from(addr - RTC_BASE), size) as u32;
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
        if self.bus.a2091.is_some() {
            // A2091 SCSI: a configured Zorro device board window (registers,
            // boot ROM, DMA strobes). Off the chip bus like Gayle.
            if let Some((crate::zorro::BoardBacking::A2091, off)) =
                self.bus.mem.zorro.device_region_at(addr, size)
            {
                self.bus.cpu_slow_external_access(Self::access_words(size));
                let (value, activity) = match self.bus.a2091.as_mut() {
                    Some(a2091) => {
                        let v = a2091.read(off, size, &mut self.bus.mem);
                        (v, a2091.take_activity())
                    }
                    None => (floating_read(size), false),
                };
                if activity {
                    self.bus.note_hdd_activity();
                }
                return value;
            }
        }
        if self.bus.akiko.is_some()
            && range_contains(crate::akiko::AKIKO_BASE, crate::akiko::AKIKO_SIZE, addr)
        {
            self.bus.cpu_slow_external_access(Self::access_words(size));
            if let Some(akiko) = self.bus.akiko.as_mut() {
                return akiko.read(addr, size, &mut self.bus.mem.chip_ram);
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
                return cdtv.read(addr, size);
            }
        }
        if range_contains(CUSTOM_BASE, CUSTOM_SIZE, addr) {
            // Custom registers live on the chip bus; custom_read itself
            // grants the contended chip-bus slot.
            return self.bus.custom_read(u64::from(addr - CUSTOM_BASE), size) as u32;
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
                value =
                    (value << 8) | self.read_sized_uncached(addr.wrapping_add(idx as u32), 1, kind);
            }
            return value;
        }

        if crate::envcfg::flag("COPPERLINE_DIAG_GAYLE")
            && (0x00D8_0000..0x00F0_0000).contains(&addr)
        {
            log::info!("float rd {addr:#08X}");
        }
        self.bus.cpu_slow_external_access(1);
        // Undriven (unmapped) read: float to the last value the chip data bus
        // carried (display/audio DMA), as on the Agnus-arbitrated chip bus --
        // not a fixed all-ones pattern. `size` is always 1 here (size>1 recurses
        // to byte reads); a 68000 byte read takes the high data-bus byte at even
        // addresses, the low byte at odd.
        let data_bus = self.bus.data_bus;
        if addr & 1 == 0 {
            u32::from(data_bus >> 8)
        } else {
            u32::from(data_bus & 0xFF)
        }
    }

    fn write_sized(&mut self, address: u32, size: usize, value: u32) {
        let addr = self.mask(address);
        if let Some(dcache) = self.dcache.as_deref_mut() {
            // Write-through with invalidate-on-hit: the write itself goes
            // to memory below; later reads refill. (The instruction cache
            // intentionally does NOT snoop writes, like real silicon.)
            dcache.invalidate_write(addr, size);
        }
        if self.overlay_rom_offset(addr, size).is_some() {
            self.bus.cpu_external_access(Self::access_words(size));
            return;
        }
        if let Some(off) = region_offset(self.bus.mem.chip_ram.len(), CHIP_RAM_BASE, addr, size) {
            self.bus
                .grant_cpu_bus_access_at(Some(addr), size, CpuBusAccessKind::Write);
            self.bus.record_cpu_chip_ram_write(off, size, value);
            write_be(&mut self.bus.mem.chip_ram, off, size, value);
            self.dbg_note_memw(addr, size);
            return;
        }
        if let Some((board, off)) = self.bus.mem.zorro.region_at(addr, size) {
            self.bus.cpu_external_access(Self::access_words(size));
            write_be(self.bus.mem.zorro.board_ram_mut(board), off, size, value);
            return;
        }
        if let Some(off) = region_offset(self.bus.mem.slow_ram.len(), SLOW_RAM_BASE, addr, size) {
            // Slow/trapdoor RAM at $C00000 is on the chip bus (reached through
            // Agnus), so CPU writes contend cycle by cycle like chip RAM rather
            // than running at uncontended external-memory speed.
            self.bus
                .grant_cpu_bus_access_at(Some(addr), size, CpuBusAccessKind::Write);
            write_be(&mut self.bus.mem.slow_ram, off, size, value);
            self.dbg_note_memw(addr, size);
            return;
        }
        if region_offset(self.bus.mem.rom.len(), ROM_BASE, addr, size).is_some() {
            self.bus.cpu_external_access(Self::access_words(size));
            return;
        }
        if region_offset(
            self.bus.mem.extended_rom.len(),
            self.bus.mem.extended_rom_base,
            addr,
            size,
        )
        .is_some()
        {
            self.bus.cpu_external_access(Self::access_words(size));
            return;
        }

        if range_contains(CIA_A_BASE, CIA_A_SIZE, addr) {
            self.bus.cpu_slow_external_access(Self::access_words(size));
            let effect = self
                .bus
                .cia_a_write(u64::from(addr - CIA_A_BASE), size, u64::from(value));
            match effect {
                CiaSideEffect::DisableOverlay => {
                    self.bus.overlay_disable_pending = true;
                    self.bus.slice_preempted = true;
                }
                CiaSideEffect::TimerStarted => {
                    self.bus.slice_preempted = true;
                }
                CiaSideEffect::KeyboardHandshakeStart
                | CiaSideEffect::KeyboardHandshakeEnd
                | CiaSideEffect::None => {}
            }
            return;
        }
        if range_contains(CIA_B_BASE, CIA_B_SIZE, addr) {
            self.bus.cpu_slow_external_access(Self::access_words(size));
            let effect = self
                .bus
                .cia_b_write(u64::from(addr - CIA_B_BASE), size, u64::from(value));
            if matches!(effect, CiaSideEffect::TimerStarted) {
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
        if self.bus.rtc_present() && range_contains(RTC_BASE, RTC_SIZE, addr) {
            self.bus.cpu_slow_external_access(Self::access_words(size));
            self.bus
                .rtc
                .write(u64::from(addr - RTC_BASE), size, u64::from(value));
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
        if self.bus.a2091.is_some() {
            if let Some((crate::zorro::BoardBacking::A2091, off)) =
                self.bus.mem.zorro.device_region_at(addr, size)
            {
                self.bus.cpu_slow_external_access(Self::access_words(size));
                let mut activity = false;
                if let Some(a2091) = self.bus.a2091.as_mut() {
                    a2091.write(off, size, value, &mut self.bus.mem);
                    activity = a2091.take_activity();
                }
                if activity {
                    self.bus.note_hdd_activity();
                }
                return;
            }
        }
        if self.bus.akiko.is_some()
            && range_contains(crate::akiko::AKIKO_BASE, crate::akiko::AKIKO_SIZE, addr)
        {
            self.bus.cpu_slow_external_access(Self::access_words(size));
            if let Some(akiko) = self.bus.akiko.as_mut() {
                akiko.write(addr, size, value, &mut self.bus.mem.chip_ram);
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
                cdtv.write(addr, size, value, &mut self.bus.mem);
                self.bus.cdtv = Some(cdtv);
            }
            return;
        }
        if range_contains(CUSTOM_BASE, CUSTOM_SIZE, addr) {
            // Custom registers live on the chip bus; custom_write itself
            // grants the contended chip-bus slot.
            if self
                .bus
                .custom_write(u64::from(addr - CUSTOM_BASE), size, u64::from(value))
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
        if crate::envcfg::flag("COPPERLINE_DIAG_GAYLE")
            && (0x00D8_0000..0x00F0_0000).contains(&addr)
        {
            log::info!("float wr {addr:#08X} <- {value:#04X}");
        }
    }

    fn overlay_rom_offset(&self, addr: u32, size: usize) -> Option<usize> {
        if !self.bus.mem.overlay {
            return None;
        }
        region_offset(self.bus.mem.rom.len(), CHIP_RAM_BASE, addr, size)
    }
}

impl AddressBus for CpuBus {
    fn read_byte(&mut self, address: u32) -> u8 {
        self.read_sized(address, 1, CpuBusAccessKind::Read) as u8
    }

    fn read_word(&mut self, address: u32) -> u16 {
        self.read_sized(address, 2, CpuBusAccessKind::Read) as u16
    }

    fn read_long(&mut self, address: u32) -> u32 {
        self.read_sized(address, 4, CpuBusAccessKind::Read)
    }

    fn write_byte(&mut self, address: u32, value: u8) {
        self.write_sized(address, 1, u32::from(value));
    }

    fn write_word(&mut self, address: u32, value: u16) {
        self.write_sized(address, 2, u32::from(value));
    }

    fn write_long(&mut self, address: u32, value: u32) {
        self.write_sized(address, 4, value);
    }

    fn read_immediate_word(&mut self, address: u32) -> u16 {
        self.read_sized(address, 2, CpuBusAccessKind::Fetch) as u16
    }

    fn read_immediate_long(&mut self, address: u32) -> u32 {
        self.read_sized(address, 4, CpuBusAccessKind::Fetch)
    }

    fn sync(&mut self, cpu_clocks: u32) {
        // Cycle-exact core (Part E.2): internal (non-bus) CPU clocks elapse
        // with the chip bus free for DMA, timed devices ticking along.
        self.bus.sync_cpu_internal_clocks(cpu_clocks);
    }

    fn interrupt_acknowledge(&mut self, level: u8) -> u32 {
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
        // (VERTB=0x20, SOFT=0x04, COPER=0x10, PORTS=0x08, EXTER=0x2000).
        // Bounded by COPPERLINE_DBG_AFTER/UNTIL.
        if crate::envcfg::flag("COPPERLINE_DBG_IRQ") {
            let secs = self.bus.emulated_seconds();
            let after = crate::envcfg::var("COPPERLINE_DBG_AFTER")
                .and_then(|s| s.trim().parse::<f64>().ok())
                .unwrap_or(0.0);
            let until = crate::envcfg::var("COPPERLINE_DBG_UNTIL")
                .and_then(|s| s.trim().parse::<f64>().ok())
                .unwrap_or(f64::INFINITY);
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

fn cpu_type_for_model(model: CpuModel) -> CpuType {
    match model {
        CpuModel::M68000 => CpuType::M68000,
        CpuModel::M68EC020 => CpuType::M68EC020,
        CpuModel::M68020 => CpuType::M68020,
        CpuModel::M68030 => CpuType::M68030,
        CpuModel::M68040 => CpuType::M68LC040,
    }
}

fn address_mask_for_model(model: CpuModel) -> u32 {
    match model {
        CpuModel::M68000 | CpuModel::M68EC020 => ADDRESS_MASK_24BIT,
        CpuModel::M68020 | CpuModel::M68030 | CpuModel::M68040 => ADDRESS_MASK_32BIT,
    }
}

fn positive_cpu_cycles(cycles: i32) -> u32 {
    cycles.max(0) as u32
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

fn floating_read(size: usize) -> u32 {
    match size {
        1 => 0xFF,
        2 => 0xFFFF,
        4 => 0xFFFF_FFFF,
        _ => 0,
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
    use crate::chipset::cia::{REG_CRA, REG_ICR, REG_TAHI, REG_TALO};
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
                rom,
                overlay: false,
                zorro,
                extended_rom: Vec::new(),
                extended_rom_base: 0,
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
                    rom,
                    overlay: true,
                    zorro: ZorroChain::default(),
                    extended_rom: Vec::new(),
                    extended_rom_base: 0,
                },
                Paula::new(Box::new(StdoutSink::new()), Box::new(NullSink)),
                FloppyController::default(),
            ),
            address_mask: ADDRESS_MASK_24BIT,
            dbg_memw_addr: None,
            dbg_memw_hit: None,
            icache: None,
            dcache: None,
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
    fn icache_removes_chip_bus_fetch_traffic_in_loops() -> Result<()> {
        // moveq #1,d0 / movec d0,cacr / moveq #10,d1 /
        // loop: nop / dbra d1,loop  -- all in chip RAM.
        let program: [u16; 8] = [
            0x7001, 0x4E7B, 0x0002, 0x720A, 0x4E71, 0x51C9, 0xFFFC, 0x4E71,
        ];
        let steps = 3 + 2 * 11;
        let run = |icache: bool| -> Result<u32> {
            let mut machine = M68kMachine::new(
                test_bus(reset_rom(0x0007_FFFE, 0x0000_0100)),
                CpuModel::M68EC020,
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
    fn rom_writes_are_ignored() {
        let mut bus = CpuBus {
            bus: test_bus(reset_rom(0, 0)),
            address_mask: ADDRESS_MASK_24BIT,
            dbg_memw_addr: None,
            dbg_memw_hit: None,
            icache: None,
            dcache: None,
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
        };
        let addr = SLOW_RAM_BASE as u32 + 64 * 1024;
        // With nothing yet driven, the bus rests at 0.
        bus.write_long(addr, 0x1234_5678);
        assert_eq!(bus.read_long(addr), 0);
        // Once a real access latches a value, undriven reads float to it; a
        // 68000 byte read takes the high data-bus byte at even addresses and the
        // low byte at odd.
        bus.bus.data_bus = 0xA53C;
        assert_eq!(bus.read_byte(addr), 0xA5);
        assert_eq!(bus.read_byte(addr + 1), 0x3C);
        assert_eq!(bus.read_word(addr), 0xA53C);
        assert_eq!(bus.read_long(addr), 0xA53C_A53C);
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
        assert_eq!(slice.instructions, 1);
        assert_eq!(machine.bus().mem.chip_ram[0x0400], 0x92);
        assert_eq!(
            slice.bus_advanced_cck,
            rom_prefetch_cck + 2 * per_chip_access,
            "TAS bus time = ROM prefetch (external, 2 cck/word) plus the two contended chip-RAM byte accesses (limit + 2 cck each under a busy BLITHOG-clear blitter)"
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
            ("CIA word data read", CIA_A_BASE),
        ] {
            let hi = ((address >> 16) & 0xFFFF) as u16;
            let lo = (address & 0xFFFF) as u16;
            let slice = run_rom_instruction(&[0x3039, hi, lo])?;
            assert_single_instruction_timing(label, slice, 8, 12);
        }
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
            let mut machine = build(bus, CpuModel::M68000, false, clocks_per_cck, false)?;
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
    fn cinv_and_cpush_are_nops_on_68030() -> Result<()> {
        // CINVA/CPUSHA decode as privileged NOPs (no cache to flush); execution
        // continues normally.
        let mut machine = machine_with_program_model(
            0x0000_0100,
            &[0xF4D8, 0xF4F8, 0x7007], // CINVA BC ; CPUSHA BC ; MOVEQ #7,D0
            CpuModel::M68030,
        )?;

        let slice = machine.step_slice(3)?;

        assert!(!slice.stopped);
        assert_eq!(machine.d(0), 7);
        assert_eq!(machine.pc(), 0x0000_0106);
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
    fn cpu_reset_instruction_clears_agnus_copper_danger_bit() -> Result<()> {
        let mut bus = test_bus_with_pc(0x0000_0100);
        write_program(
            &mut bus,
            0x0000_0100,
            &[
                0x4E70, // RESET
                0x4E71, // NOP, proves the CPU itself was not reset.
            ],
        );
        bus.agnus.write_copcon(0x0002);

        let mut machine = M68kMachine::new(bus, CpuModel::M68000, false)?;
        assert_ne!(machine.bus().agnus.copcon, 0);
        let slice = machine.step_slice(1)?;

        assert_eq!(slice.instructions, 1);
        assert_eq!(machine.bus().agnus.copcon, 0);
        assert_eq!(machine.pc(), 0x0000_0102);
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
        let mut machine = build(bus, CpuModel::M68000, false, 2, false)?;

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

        crate::savestate::save(&machine, &state_t1)?;
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
        crate::savestate::save(&machine, &state_t2)?;
        // The loop actually ran: the counter advanced and frames elapsed.
        assert!(original.last().unwrap().3 > original.first().unwrap().3);

        // Rewind the same machine back to T1 and replay the same steps.
        crate::savestate::load(&mut machine, &state_t1)?;
        let replay = run_trace(&mut machine)?;
        crate::savestate::save(&machine, &state_t2_replay)?;

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
        emu.bus_mut().input.add_mouse_delta_port1(40, 0);
        emu.tt_note_input(ReplayAction::MouseMove { dx: 40, dy: 0 });

        while emu.bus().emulated_frames() < 6 {
            emu.step_frame()?;
        }
        let pos_now = emu.retired_instructions();
        let counter_with_input = emu.bus().input.mouse_x_port1;
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
            emu.bus().input.mouse_x_port1,
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
}
