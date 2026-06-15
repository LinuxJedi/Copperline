// SPDX-License-Identifier: GPL-3.0-or-later

//! Top-level emulator: owns the M68K CPU instance and drives execution
//! in fixed-size instruction slices, advancing the raster after each
//! slice and raising chipset/CIA/Paula interrupts.

use crate::audio::{audio_profile_enabled, AudioRuntimeStatus};
use crate::bus::Bus;
use crate::config::{CpuModel, PacingBudget, SpeedMode};
use crate::cpu;
use anyhow::Result;
use std::time::{Duration, Instant};

const INSTRUCTIONS_PER_SLICE: usize = 32_000;
const INSTRUCTIONS_PER_REALTIME_SLICE: usize = 8_192;
/// Approximate CPU cycles per emulated M68000 instruction for converting
/// frame-sized instruction budgets and real-mode device cadence. The
/// instruction-paced backend is not cycle-exact, so use the 68000's
/// minimum instruction timing as the default instead of over-advancing
/// Agnus/Denise/Paula between retired instructions.
const DEFAULT_CPU_CYCLES_PER_INSTRUCTION: f64 = 4.0;
const CPU_CYCLES_PER_COLOR_CLOCK: f64 = 2.0;
/// Stock PAL Amiga 68000 clock. Copperline is instruction-paced rather
/// than cycle-exact, so real mode divides this by the current
/// cycles-per-instruction approximation.
const PAL_68000_CLOCK_HZ: f64 = 7_093_790.0;
const REAL_PACING_PROFILE_ENV: &str = "COPPERLINE_REAL_PACING_PROFILE";
const REAL_PACING_BUDGET_ENV: &str = "COPPERLINE_REAL_PACING_BUDGET";
// Largest wall-clock deficit the real-time pacer will try to chase before it
// re-anchors instead. Beyond this (roughly a couple of frames at 50/60 Hz) the
// lag is treated as an unrecoverable stall (paused dialog, debugger break,
// GC/host hitch) and the pacing anchor is advanced rather than fast-forwarding
// the emulator to catch up.
const MAX_REALTIME_CATCHUP: Duration = Duration::from_millis(100);

/// Cached COPPERLINE_DIAG_CCK gate (read once). Checked at every CPU-slice boundary,
/// which is far too frequent for a live env lookup.
fn diag_cck_on() -> bool {
    use std::sync::OnceLock;
    static V: OnceLock<bool> = OnceLock::new();
    *V.get_or_init(|| crate::envcfg::flag("COPPERLINE_DIAG_CCK"))
}

pub struct Emulator {
    pub machine: cpu::M68kMachine,
    pub stats: EmuStats,
    timing_mode: RuntimeTiming,
    /// When true, pace presentation to wall-clock time (interactive
    /// window). When false, run the deterministic core unthrottled
    /// (headless screenshot/frame-dump runs). The emulated result is
    /// identical either way.
    paced: bool,
    cpu_cycles_per_instruction: f64,
    real_pacing_budget_mode: RealPacingBudgetMode,
    audio_profile: AudioRuntimeProfile,
    real_pacing_profile: RealPacingProfile,
}

struct ExecutedSlice {
    actual_instructions: usize,
    actual_cpu_cycles: u32,
    actual_cpu_cck: u32,
    bus_advanced_cck: u32,
    cpu_stopped: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct RealSliceAccounting {
    budget_debit: usize,
    device_cck: u32,
    chip_bus_wait_cck: u32,
    slice_cck: u32,
}

struct AudioRuntimeProfile {
    enabled: bool,
    sleep_count: u64,
    sleep_nanos: u128,
    last_log: Instant,
}

struct RealPacingProfile {
    enabled: bool,
    retired_instructions: u64,
    m68k_cycles: u64,
    chip_bus_wait_cck: u64,
    device_cck: u64,
    sleep_count: u64,
    sleep_nanos: u128,
    wall_overrun_count: u64,
    wall_overrun_nanos: u128,
    last_cpu_chip_slots: u64,
    last_log: Instant,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RealPacingBudgetMode {
    RetiredInstructions,
    M68kCycles,
}

impl AudioRuntimeProfile {
    fn new() -> Self {
        Self {
            enabled: audio_profile_enabled(),
            sleep_count: 0,
            sleep_nanos: 0,
            last_log: Instant::now(),
        }
    }

    fn record_sleep(&mut self, elapsed: Duration) {
        if !self.enabled {
            return;
        }
        self.sleep_count = self.sleep_count.saturating_add(1);
        self.sleep_nanos = self.sleep_nanos.saturating_add(elapsed.as_nanos());
        self.log_if_due();
    }

    fn log_if_due(&mut self) {
        if !self.enabled || self.last_log.elapsed().as_secs() < 1 {
            return;
        }
        log::info!(
            "audio profile: emulator_sleep_count={} emulator_sleep_time_ms={:.3}",
            self.sleep_count,
            self.sleep_nanos as f64 / 1_000_000.0,
        );
        self.sleep_count = 0;
        self.sleep_nanos = 0;
        self.last_log = Instant::now();
    }
}

impl RealPacingProfile {
    fn new() -> Self {
        Self {
            enabled: real_pacing_profile_enabled(),
            retired_instructions: 0,
            m68k_cycles: 0,
            chip_bus_wait_cck: 0,
            device_cck: 0,
            sleep_count: 0,
            sleep_nanos: 0,
            wall_overrun_count: 0,
            wall_overrun_nanos: 0,
            last_cpu_chip_slots: 0,
            last_log: Instant::now(),
        }
    }

    #[cfg(test)]
    fn enabled_for_test() -> Self {
        Self {
            enabled: true,
            ..Self::new()
        }
    }

    fn record_slice(&mut self, run: &ExecutedSlice, accounting: RealSliceAccounting) {
        if !self.enabled {
            return;
        }
        self.retired_instructions = self
            .retired_instructions
            .saturating_add(run.actual_instructions as u64);
        self.m68k_cycles = self
            .m68k_cycles
            .saturating_add(u64::from(run.actual_cpu_cycles));
        self.chip_bus_wait_cck = self
            .chip_bus_wait_cck
            .saturating_add(u64::from(accounting.chip_bus_wait_cck));
        self.device_cck = self
            .device_cck
            .saturating_add(u64::from(accounting.device_cck));
    }

    fn record_sleep(&mut self, elapsed: Duration) {
        if !self.enabled {
            return;
        }
        self.sleep_count = self.sleep_count.saturating_add(1);
        self.sleep_nanos = self.sleep_nanos.saturating_add(elapsed.as_nanos());
    }

    fn record_wall_overrun(&mut self, elapsed: Duration) {
        if !self.enabled {
            return;
        }
        self.wall_overrun_count = self.wall_overrun_count.saturating_add(1);
        self.wall_overrun_nanos = self.wall_overrun_nanos.saturating_add(elapsed.as_nanos());
    }

    fn log_if_due(&mut self, audio_status: AudioRuntimeStatus, cpu_chip_slots_cumulative: u64) {
        if !self.enabled || self.last_log.elapsed().as_secs() < 1 {
            return;
        }
        let cpu_chip_slots_delta = cpu_chip_slots_cumulative.wrapping_sub(self.last_cpu_chip_slots);
        self.last_cpu_chip_slots = cpu_chip_slots_cumulative;
        log::info!(
            "real pacing: retired={} m68k_cycles={} chip_wait_cck={} device_cck={} cpu_chip_slots={} sleep_count={} sleep_ms={:.3} wall_late_count={} wall_late_ms={:.3} audio_queue_frames={} audio_lead_ms={:.1} audio_underruns={} audio_overruns={} audio_stale_frames={}",
            self.retired_instructions,
            self.m68k_cycles,
            self.chip_bus_wait_cck,
            self.device_cck,
            cpu_chip_slots_delta,
            self.sleep_count,
            self.sleep_nanos as f64 / 1_000_000.0,
            self.wall_overrun_count,
            self.wall_overrun_nanos as f64 / 1_000_000.0,
            audio_status.queue_depth_frames,
            audio_status.output_lead_seconds * 1_000.0,
            audio_status.callback_underrun_frames,
            audio_status.dropped_overrun_frames,
            audio_status.skipped_stale_frames,
        );
        self.retired_instructions = 0;
        self.m68k_cycles = 0;
        self.chip_bus_wait_cck = 0;
        self.device_cck = 0;
        self.sleep_count = 0;
        self.sleep_nanos = 0;
        self.wall_overrun_count = 0;
        self.wall_overrun_nanos = 0;
        self.last_log = Instant::now();
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeTiming {
    /// The single emulation timing: CPU and chipset/audio advance together
    /// in emulated time. (Turbo's host-time-paced slicing was removed; it
    /// was non-deterministic and landed CPU register writes at jittery beam
    /// positions.)
    Real,
}

impl RuntimeTiming {
    fn from_config(speed_mode: SpeedMode, _live_audio: bool) -> Self {
        match speed_mode {
            SpeedMode::Real => Self::Real,
        }
    }

    fn realtime_budget(self, cpu_cycles_per_instruction: f64) -> (usize, f64) {
        let target = real_target_instructions_per_second(cpu_cycles_per_instruction);
        ((target / 60.0).round().max(1.0) as usize, target)
    }
}

fn real_cpu_cycles_per_instruction() -> f64 {
    crate::envcfg::var("COPPERLINE_REAL_CPU_CPI")
        .and_then(|raw| raw.parse::<f64>().ok())
        .filter(|value| value.is_finite() && *value > 0.0)
        .unwrap_or(DEFAULT_CPU_CYCLES_PER_INSTRUCTION)
}

fn real_pacing_profile_enabled() -> bool {
    crate::envcfg::flag(REAL_PACING_PROFILE_ENV)
}

/// Resolve the pacing budget mode: the `COPPERLINE_REAL_PACING_BUDGET` env var
/// overrides the per-config default when it names a recognized mode; an
/// unrecognized value is warned about and ignored (the config default
/// stands).
fn real_pacing_budget_mode(config_default: RealPacingBudgetMode) -> RealPacingBudgetMode {
    let raw = crate::envcfg::var(REAL_PACING_BUDGET_ENV);
    match parse_real_pacing_budget_mode(raw.as_deref()) {
        Some(mode) => mode,
        None => {
            if raw.is_some() {
                log::warn!(
                    "{} ignored; expected `instructions` or `cycles`",
                    REAL_PACING_BUDGET_ENV
                );
            }
            config_default
        }
    }
}

/// Parse an explicit pacing-budget selector. Returns `None` when the value
/// is absent or unrecognized so the caller can fall back to its default.
fn parse_real_pacing_budget_mode(raw: Option<&str>) -> Option<RealPacingBudgetMode> {
    match raw {
        Some("cycles") | Some("m68k-cycles") => Some(RealPacingBudgetMode::M68kCycles),
        Some("instructions") | Some("retired-instructions") => {
            Some(RealPacingBudgetMode::RetiredInstructions)
        }
        None | Some(_) => None,
    }
}

impl From<PacingBudget> for RealPacingBudgetMode {
    fn from(budget: PacingBudget) -> Self {
        match budget {
            PacingBudget::Cycles => RealPacingBudgetMode::M68kCycles,
            PacingBudget::Instructions => RealPacingBudgetMode::RetiredInstructions,
        }
    }
}

fn real_target_instructions_per_second(cpu_cycles_per_instruction: f64) -> f64 {
    PAL_68000_CLOCK_HZ / cpu_cycles_per_instruction
}

#[derive(Default)]
pub struct EmuStats {
    pub frames: u64,
    pub slices: u64,
    pub instructions: u64,
    pub started_at: Option<std::time::Instant>,
}

impl Emulator {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        bus: Bus,
        cpu_model: CpuModel,
        fpu_enabled: bool,
        speed_mode: SpeedMode,
        pacing_budget: PacingBudget,
        cpu_clocks_per_cck: u32,
        live_audio: bool,
        paced: bool,
    ) -> Result<Self> {
        let timing_mode = RuntimeTiming::from_config(speed_mode, live_audio);
        let cpu_clocks_per_cck = cpu_clocks_per_cck.max(1);
        // The pacing math is expressed against the stock 2-clocks-per-CCK
        // 68000 ratio. An accelerated CPU runs at `cpu_clocks_per_cck` clocks
        // per colour clock, i.e. it retires instructions faster relative to
        // the chipset. That is exactly equivalent, for every pacing formula
        // (target instructions/sec, instruction<->CCK conversions), to a CPU
        // with `CPU_CYCLES_PER_COLOR_CLOCK / cpu_clocks_per_cck` as much
        // per-instruction cost, so fold the speed multiple into the effective
        // cycles-per-instruction and leave the pacing helpers untouched. With
        // the stock ratio of 2 this is an identity (factor 1.0).
        let speed_factor = CPU_CYCLES_PER_COLOR_CLOCK / cpu_clocks_per_cck as f64;
        let cpu_cycles_per_instruction = real_cpu_cycles_per_instruction() * speed_factor;
        if cpu_clocks_per_cck != 2 {
            log::info!(
                "cpu speed: {:.2} MHz ({}x colour clock), fast RAM at CPU speed",
                cpu_clocks_per_cck as f64 * 3.546895,
                cpu_clocks_per_cck
            );
        }
        let real_pacing_budget_mode = real_pacing_budget_mode(pacing_budget.into());
        if real_pacing_budget_mode == RealPacingBudgetMode::M68kCycles {
            log::info!("real pacing budget: returned m68k cycles plus explicit chip-bus waits");
        }
        let mut bus = bus;
        // The chipset/CIA/Paula advance in emulated time, not wall-clock.
        bus.set_realtime_devices_enabled(false);
        let machine = cpu::build(bus, cpu_model, fpu_enabled, cpu_clocks_per_cck, false)?;
        Ok(Self {
            machine,
            stats: EmuStats::default(),
            timing_mode,
            paced,
            cpu_cycles_per_instruction,
            real_pacing_budget_mode,
            audio_profile: AudioRuntimeProfile::new(),
            real_pacing_profile: RealPacingProfile::new(),
        })
    }

    /// Install the opt-in 68020/030 CACR-controlled cache models.
    pub fn set_cache_emulation(&mut self, icache: bool, dcache: bool) {
        self.machine.set_cache_emulation(icache, dcache);
    }

    pub fn bus(&self) -> &Bus {
        self.machine.bus()
    }

    pub fn bus_mut(&mut self) -> &mut Bus {
        self.machine.bus_mut()
    }

    /// Suspend only host live audio output. Emulated Paula time still
    /// advances whenever the machine is stepped.
    pub fn set_live_audio_suspended(&mut self, suspended: bool) {
        self.bus_mut().set_live_audio_suspended(suspended);
    }

    pub fn keyboard_reset(&mut self) -> Result<()> {
        log::info!("keyboard reset pulse");
        self.bus_mut().reset_for_keyboard_reset();
        self.machine.reset_after_bus_reset();
        self.stats = EmuStats::default();
        Ok(())
    }

    /// Cold power-on reset: clears RAM and returns the machine to its
    /// fresh power-cycled state, distinct from the warm keyboard reset.
    pub fn power_on_reset(&mut self) -> Result<()> {
        log::info!("cold power-on reset");
        self.bus_mut().power_on_reset();
        self.machine.reset_after_bus_reset();
        self.stats = EmuStats::default();
        Ok(())
    }

    /// Fit a new boot ROM (and optionally an extended ROM) and cold-reset,
    /// as if the Kickstart had been physically swapped and power cycled.
    /// Both images are validated before anything is mutated, so on error the
    /// running machine keeps its current ROMs. `extended` of `None` removes
    /// any fitted extended ROM.
    pub fn reload_rom(&mut self, rom: Vec<u8>, extended: Option<Vec<u8>>) -> Result<()> {
        // Accept a 256 KiB Kickstart 1.x part by mirroring it up to the full
        // 512 KiB ROM window, matching how it decodes on real hardware.
        let rom = crate::memory::normalize_boot_rom(rom)?;
        // Validate the extended-ROM size up front so a bad image cannot
        // leave the main ROM swapped but the extended ROM half-applied.
        if let Some(image) = &extended {
            if !matches!(image.len(), 0x8_0000 | 0x4_0000) {
                anyhow::bail!(
                    "extended ROM is {} bytes; expected 512 KiB ($E00000) \
                     or 256 KiB ($F00000)",
                    image.len()
                );
            }
        }
        let mem = &mut self.bus_mut().mem;
        mem.rom = rom;
        match extended {
            Some(image) => mem.attach_extended_rom(image)?,
            None => mem.detach_extended_rom(),
        }
        log::info!("boot ROM replaced; cold-resetting");
        self.power_on_reset()
    }

    /// Write a save state of the whole emulated machine to `path`. Call
    /// between frames (the event loop and the headless frame loop both run
    /// at frame granularity, so any caller outside step_frame qualifies).
    pub fn save_state(&self, path: &std::path::Path) -> Result<()> {
        crate::savestate::save(&self.machine, path)
    }

    /// Restore a save state from `path`. On success emulated time jumps to
    /// the state's timeline, so the real-time pacing anchor is re-baselined
    /// to "now"; on failure the running machine is untouched.
    pub fn load_state(&mut self, path: &std::path::Path) -> Result<()> {
        crate::savestate::load(&mut self.machine, path)?;
        self.reanchor_realtime_clock();
        Ok(())
    }

    /// Re-baseline the real-time pacing anchor so the next frame paces from
    /// "now" instead of trying to catch up an accumulated wall-clock deficit.
    ///
    /// Call this when resuming after a deliberate pause where wall time
    /// advanced but emulated time did not (e.g. a modal file dialog blocking
    /// the main thread). Without re-anchoring, the pacer would see emulated
    /// time far behind the wall-clock target and fast-forward the emulator in
    /// a catch-up burst, corrupting audio/video pacing. The anchor is placed
    /// so that the current emulated device target maps exactly to now. No-op
    /// until the clock has been anchored by the first step_frame.
    pub fn reanchor_realtime_clock(&mut self) {
        if self.stats.started_at.is_none() {
            return;
        }
        let target_seconds = (self.bus().emulated_seconds()
            - self.bus().live_audio_output_lead_seconds().max(0.0))
        .max(0.0);
        let now = Instant::now();
        self.stats.started_at = now.checked_sub(Duration::from_secs_f64(target_seconds));
        if self.stats.started_at.is_none() {
            // Saturated below the epoch (extreme target); fall back to now.
            self.stats.started_at = Some(now);
        }
    }

    /// Whether presentation is paced to wall-clock time (false = warp).
    pub fn paced(&self) -> bool {
        self.paced
    }

    /// Enable/disable wall-clock pacing (the UI's warp-speed toggle).
    /// Re-enabling re-anchors the pacing clock so the emulator does not
    /// sprint to catch up the time spent in warp.
    pub fn set_paced(&mut self, paced: bool) {
        if self.paced == paced {
            return;
        }
        self.paced = paced;
        if paced {
            self.reanchor_realtime_clock();
        }
    }

    /// Execute exactly `count` CPU instructions (interactive debugger
    /// single-step). The cycle-exact core advances the chipset in lockstep,
    /// so device state stays consistent; no wall-clock pacing is applied.
    pub fn debug_step_instructions(&mut self, count: usize) -> Result<()> {
        for _ in 0..count {
            self.execute_cpu_slice(1)?;
            self.machine.refresh_irq_line();
        }
        Ok(())
    }

    /// Run until the CPU reaches `target_pc` (masked to the bus width), up
    /// to `max_instructions`. Returns true when the target was hit.
    pub fn debug_run_to_pc(&mut self, target_pc: u32, max_instructions: usize) -> Result<bool> {
        const PC_MASK: u32 = 0x00FF_FFFF;
        let target = target_pc & PC_MASK;
        for _ in 0..max_instructions {
            let run = self.execute_cpu_slice(1)?;
            self.machine.refresh_irq_line();
            if run.cpu_stopped {
                // A stopped CPU makes no progress under single-instruction
                // slices; advance devices to the next event so the wake-up
                // interrupt can fire, mirroring step_real's fast-forward.
                let chunk = self.idle_fast_forward_chunk(INSTRUCTIONS_PER_REALTIME_SLICE);
                let run = self.execute_cpu_slice(chunk)?;
                let accounting = real_slice_accounting(
                    &run,
                    chunk,
                    self.cpu_cycles_per_instruction,
                    self.real_pacing_budget_mode,
                );
                if run.cpu_stopped {
                    let idle_cck = accounting.slice_cck.saturating_sub(run.bus_advanced_cck);
                    self.bus_mut().advance_devices(idle_cck);
                }
                self.machine.refresh_irq_line();
            }
            if self.machine.pc() & PC_MASK == target {
                return Ok(true);
            }
            // A breakpoint/watch hit on the way to the target ends the
            // run; the window reports the hit instead of "not reached".
            if self.machine.ui_debug_stop_pending() {
                return Ok(false);
            }
        }
        Ok(false)
    }

    pub fn step_frame(&mut self) -> Result<()> {
        if self.stats.started_at.is_none() {
            self.stats.started_at = Some(std::time::Instant::now());
        }
        match self.timing_mode {
            RuntimeTiming::Real => self.step_real()?,
        }
        self.stats.frames += 1;
        if crate::envcfg::flag("COPPERLINE_DIAG_PCSAMPLE") && self.stats.frames.is_multiple_of(50) {
            log::info!(
                "pcsample frame={} pc={:#010X} sr={:#06X}",
                self.stats.frames,
                self.machine.pc(),
                self.machine.sr()
            );
        }
        Ok(())
    }

    fn step_real(&mut self) -> Result<()> {
        let mut remaining = self.instruction_budget();
        // Cycle-step while the CPU is actively executing so every chip
        // register write lands at the correct beam position (one
        // instruction per slice, with the chipset advanced for that
        // instruction immediately after it retires). While the CPU is
        // halted in STOP it writes nothing, so fast-forward to the next
        // device event instead of stepping one instruction at a time --
        // then drop straight back to single-instruction stepping so the
        // wake-up interrupt handler, which often performs mid-frame
        // display writes, is cycle-accurate too.
        let mut cpu_idle = false;
        while remaining > 0 {
            let chunk = if cpu_idle {
                self.idle_fast_forward_chunk(remaining)
            } else {
                1
            };
            let run = self.execute_cpu_slice(chunk)?;
            let accounting = real_slice_accounting(
                &run,
                chunk,
                self.cpu_cycles_per_instruction,
                self.real_pacing_budget_mode,
            );
            remaining = remaining.saturating_sub(accounting.budget_debit);
            if run.cpu_stopped {
                // A stopped CPU performed no bus activity; advance the chipset
                // and timed devices through the idle period. (A running slice
                // needs nothing here: the cycle-exact core already advanced
                // its full device time through sync/grant as it executed.)
                let idle_cck = accounting.slice_cck.saturating_sub(run.bus_advanced_cck);
                self.bus_mut().advance_devices(idle_cck);
            }
            self.machine.refresh_irq_line();
            self.real_pacing_profile.record_slice(&run, accounting);
            // An interactive breakpoint/watch hit ends the frame early;
            // the window surfaces it and pauses. (Checked after the device
            // advance so a hit during an idle fast-forward is seen too.)
            if self.machine.ui_debug_stop_pending() {
                break;
            }
            // Only a single-instruction slice that came back stopped tells
            // us the CPU is genuinely idle; never batch on the slice right
            // after a fast-forward, so a wake-up is always stepped.
            cpu_idle = run.cpu_stopped && chunk == 1;
        }
        // Pace presentation to wall-clock only for the interactive window;
        // headless runs advance the deterministic core unthrottled.
        if self.paced {
            self.sleep_until_realtime_device_time();
        }
        Ok(())
    }

    /// Largest instruction budget to skip while the CPU is halted in STOP.
    /// Bounded to the next device event that could raise an interrupt (and
    /// to the frame boundary) so the chipset advances only up to that
    /// event -- the CPU then wakes at the correct beam position and the
    /// handler is cycle-stepped from there.
    fn idle_fast_forward_chunk(&self, remaining: usize) -> usize {
        let mut chunk = remaining
            .min(INSTRUCTIONS_PER_SLICE)
            .min(INSTRUCTIONS_PER_REALTIME_SLICE);
        let bus = self.bus();
        let cap_cck = |cck: u32, chunk: &mut usize| {
            *chunk = (*chunk).min(self.instructions_for_cck(cck).max(1));
        };
        if let Some(ticks) = bus
            .cia_a
            .next_underflow_ticks()
            .into_iter()
            .chain(bus.cia_b.next_underflow_ticks())
            .min()
        {
            chunk = chunk.min(self.instructions_for_cia_ticks(ticks).max(1));
        }
        if let Some(cck) = bus.floppy.next_completion_cck(bus.agnus.dmacon) {
            cap_cck(cck, &mut chunk);
        }
        if let Some(cck) = bus.floppy.next_sync_irq_cck(bus.agnus.dmacon) {
            cap_cck(cck, &mut chunk);
        }
        if let Some(cck) = bus.floppy.next_index_pulse_cck() {
            cap_cck(cck, &mut chunk);
        }
        if let Some(cck) = bus.next_copper_wakeup_cck() {
            cap_cck(cck, &mut chunk);
        }
        if let Some(cck) = bus.next_blitter_completion_cck() {
            cap_cck(cck, &mut chunk);
        }
        if let Some(cck) = bus.next_serial_event_cck() {
            cap_cck(cck, &mut chunk);
        }
        if let Some(cck) = bus.next_keyboard_event_cck() {
            cap_cck(cck, &mut chunk);
        }
        if let Some(cck) = bus.next_pot_event_cck() {
            cap_cck(cck, &mut chunk);
        }
        if let Some(cck) = bus.next_audio_irq_cck() {
            cap_cck(cck, &mut chunk);
        }
        cap_cck(bus.next_frame_event_cck(), &mut chunk);
        if let Some(cck) = bus.next_display_start_event_cck() {
            cap_cck(cck, &mut chunk);
        }
        if let Some(cck) = bus.next_cia_b_tod_alarm_cck() {
            cap_cck(cck, &mut chunk);
        }
        chunk.max(1)
    }

    pub fn report_stats(&self) {
        let elapsed = self
            .stats
            .started_at
            .map(|t| t.elapsed().as_secs_f64())
            .unwrap_or(0.0);
        if elapsed > 0.0 {
            let inst = self.stats.instructions as f64;
            let emulated = self.bus().emulated_seconds();
            log::info!(
                "emu stats: {:.1}s elapsed, {:.1}s emulated ({:.1}%), {} frames ({:.1}/s), {} slices ({:.1}/s), ~{:.2} MIPS",
                elapsed,
                emulated,
                emulated / elapsed * 100.0,
                self.stats.frames,
                self.stats.frames as f64 / elapsed,
                self.stats.slices,
                self.stats.slices as f64 / elapsed,
                inst / elapsed / 1e6,
            );
        }
        self.bus().dump_video_pipeline_stats("emu stats");
    }

    fn instruction_budget(&mut self) -> usize {
        let (quantum, _target) = self
            .timing_mode
            .realtime_budget(self.cpu_cycles_per_instruction);
        quantum
    }

    fn instructions_for_cia_ticks(&self, ticks: u32) -> usize {
        self.instructions_for_cck(ticks.saturating_mul(5))
    }

    fn instructions_for_cck(&self, cck: u32) -> usize {
        instructions_for_cck_value(cck, self.cpu_cycles_per_instruction)
    }

    fn sleep_until_realtime_device_time(&mut self) {
        let Some(started_at) = self.stats.started_at else {
            return;
        };
        let now = Instant::now();
        let target = realtime_device_time_target(
            started_at,
            self.bus().emulated_seconds(),
            self.bus().live_audio_output_lead_seconds(),
        );
        if let Some(wait) = target.and_then(|target| target.checked_duration_since(now)) {
            let sleep_started = Instant::now();
            std::thread::sleep(wait);
            let elapsed = sleep_started.elapsed();
            self.audio_profile.record_sleep(elapsed);
            self.real_pacing_profile.record_sleep(elapsed);
        } else {
            if let Some(lag) = target.and_then(|target| now.checked_duration_since(target)) {
                if lag > Duration::ZERO {
                    self.real_pacing_profile.record_wall_overrun(lag);
                    // Self-heal against large stalls. When emulated device
                    // time falls behind the wall-clock target by more than a
                    // couple of frames (a paused file dialog, debugger break,
                    // GC or host hitch, or a deferred-insert wall divergence),
                    // chasing the deficit would fast-forward the emulator and
                    // wreck audio/video pacing. Drop the unrecoverable excess
                    // by advancing the pacing anchor forward by the lag, so we
                    // resume pacing from "now" instead of sprinting to catch
                    // up. The overrun telemetry above is still recorded.
                    if lag > MAX_REALTIME_CATCHUP {
                        if let Some(anchor) = self.stats.started_at {
                            self.stats.started_at = Some(anchor + lag);
                        }
                    }
                }
            }
            self.audio_profile.log_if_due();
        }
        let audio_status = self.bus().live_audio_status();
        let cpu_chip_slots = self.bus().cpu_granted_chip_slots();
        self.real_pacing_profile
            .log_if_due(audio_status, cpu_chip_slots);
    }

    fn execute_cpu_slice(&mut self, chunk: usize) -> Result<ExecutedSlice> {
        let run = self.machine.step_slice(chunk)?;
        self.stats.slices += 1;
        let actual_instructions = run.instructions;
        let actual_cpu_cycles = run.cpu_cycles;
        let actual_cpu_cck = run.cpu_cck;
        // COPPERLINE_DIAG_CCK: compare canonical core cycle count (cpu_cck) vs the
        // actual beam advance (bus_advanced_cck) per slice, to detect cycle-model
        // over/under-timing. Logs instructions too so cck-per-instr is visible.
        if diag_cck_on() && actual_instructions > 0 {
            log::info!(
                "cck f={} instr={} cpu_cck={} bus_cck={} delta={}",
                self.bus().emulated_frames(),
                actual_instructions,
                actual_cpu_cck,
                run.bus_advanced_cck,
                run.bus_advanced_cck as i64 - actual_cpu_cck as i64,
            );
        }
        self.stats.instructions = self
            .stats
            .instructions
            .saturating_add(actual_instructions as u64);
        if self.bus().overlay_disable_pending {
            self.machine.disable_overlay();
        }
        if self.bus().keyboard_system_reset_pending {
            // The keyboard MCU completed its 500 ms KCLK reset hold
            // (Ctrl+Amiga+Amiga): hard-reset the machine. The reset
            // path restarts the MCU's own power-up flow.
            self.bus_mut().keyboard_system_reset_pending = false;
            log::info!("keyboard KCLK reset (Ctrl+Amiga+Amiga)");
            self.bus_mut().reset_for_keyboard_reset();
            self.machine.reset_after_bus_reset();
            self.stats = EmuStats::default();
        }

        Ok(ExecutedSlice {
            actual_instructions,
            actual_cpu_cycles,
            actual_cpu_cck,
            bus_advanced_cck: run.bus_advanced_cck,
            cpu_stopped: run.stopped,
        })
    }
}

fn real_slice_accounting(
    run: &ExecutedSlice,
    requested_instructions: usize,
    cpu_cycles_per_instruction: f64,
    budget_mode: RealPacingBudgetMode,
) -> RealSliceAccounting {
    if run.cpu_stopped {
        // A stopped CPU performs no bus activity; the idle period is paced by
        // the requested fast-forward span and advanced post-hoc by step_real.
        let device_cck = run.actual_cpu_cck.max(cck_for_instructions(
            requested_instructions,
            cpu_cycles_per_instruction,
        ));
        return RealSliceAccounting {
            budget_debit: requested_instructions.max(run.actual_instructions).max(1),
            device_cck,
            chip_bus_wait_cck: 0,
            slice_cck: device_cck.max(run.bus_advanced_cck),
        };
    }

    // Cycle-exact core (m68k Part E): the bus time that elapsed during the
    // slice IS the device time -- internal CPU clocks (sync), bus-cycle
    // tails, chip-bus grants and contention waits were all advanced (and
    // timed devices ticked) as they happened. No floor or reconciliation.
    let device_cck = run.bus_advanced_cck;
    let budget_debit = match budget_mode {
        RealPacingBudgetMode::RetiredInstructions => run.actual_instructions.max(1),
        RealPacingBudgetMode::M68kCycles => {
            instructions_for_cck_value(device_cck, cpu_cycles_per_instruction).max(1)
        }
    };
    RealSliceAccounting {
        budget_debit,
        device_cck,
        chip_bus_wait_cck: 0,
        slice_cck: device_cck,
    }
}

fn cck_for_instructions(instructions: usize, cpu_cycles_per_instruction: f64) -> u32 {
    ((instructions as f64 * cpu_cycles_per_instruction / CPU_CYCLES_PER_COLOR_CLOCK).ceil())
        .clamp(0.0, u32::MAX as f64) as u32
}

fn instructions_for_cck_value(cck: u32, cpu_cycles_per_instruction: f64) -> usize {
    ((cck as f64 * CPU_CYCLES_PER_COLOR_CLOCK / cpu_cycles_per_instruction).ceil())
        .clamp(0.0, usize::MAX as f64) as usize
}

#[cfg(test)]
fn realtime_device_time_wait(
    started_at: Instant,
    now: Instant,
    emulated_seconds: f64,
    live_output_lead_seconds: f64,
) -> Option<Duration> {
    realtime_device_time_target(started_at, emulated_seconds, live_output_lead_seconds)?
        .checked_duration_since(now)
}

fn realtime_device_time_target(
    started_at: Instant,
    emulated_seconds: f64,
    live_output_lead_seconds: f64,
) -> Option<Instant> {
    let target_seconds = (emulated_seconds - live_output_lead_seconds.max(0.0)).max(0.0);
    started_at.checked_add(Duration::from_secs_f64(target_seconds))
}

#[cfg(test)]
mod tests {
    use super::{
        cck_for_instructions, instructions_for_cck_value, real_slice_accounting, ExecutedSlice,
        RealPacingBudgetMode, RealPacingProfile, RuntimeTiming, DEFAULT_CPU_CYCLES_PER_INSTRUCTION,
    };
    use crate::audio::AudioRuntimeStatus;

    use crate::config::{PacingBudget, SpeedMode};
    use std::time::{Duration, Instant};

    #[test]
    fn default_real_cpu_timing_maps_cycles_to_color_clocks() {
        // Default model: 4 CPU cycles per instruction, 2 CPU cycles per
        // color clock -> two instructions span ceil(2*4/2) = 4 color
        // clocks, and four color clocks map back to two instructions.
        assert_eq!(
            cck_for_instructions(2, DEFAULT_CPU_CYCLES_PER_INSTRUCTION),
            4
        );
        assert_eq!(
            instructions_for_cck_value(4, DEFAULT_CPU_CYCLES_PER_INSTRUCTION),
            2
        );
    }

    #[test]
    fn stopped_real_slice_debits_budget_and_advances_devices() {
        let run = ExecutedSlice {
            actual_instructions: 0,
            actual_cpu_cycles: 0,
            actual_cpu_cck: 0,
            bus_advanced_cck: 0,
            cpu_stopped: true,
        };

        let accounting = real_slice_accounting(
            &run,
            4096,
            DEFAULT_CPU_CYCLES_PER_INSTRUCTION,
            RealPacingBudgetMode::RetiredInstructions,
        );
        assert_eq!(accounting.budget_debit, 4096);
        assert_eq!(accounting.chip_bus_wait_cck, 0);
        assert_eq!(
            accounting.slice_cck,
            cck_for_instructions(4096, DEFAULT_CPU_CYCLES_PER_INSTRUCTION)
        );
    }

    #[test]
    fn running_real_slice_is_instruction_paced_not_cycle_throttled() {
        // RetiredInstructions mode debits the budget by the retired instruction
        // count regardless of how much device (bus) time the slice consumed: the
        // 20 cck of bus_advanced_cck do not throttle the 10-instruction debit.
        let run = ExecutedSlice {
            actual_instructions: 10,
            actual_cpu_cycles: 2000,
            actual_cpu_cck: 1000,
            bus_advanced_cck: 20,
            cpu_stopped: false,
        };

        let accounting = real_slice_accounting(
            &run,
            4096,
            DEFAULT_CPU_CYCLES_PER_INSTRUCTION,
            RealPacingBudgetMode::RetiredInstructions,
        );
        assert_eq!(accounting.budget_debit, 10);
        assert_eq!(accounting.chip_bus_wait_cck, 0);
        // For a running slice the device time IS the bus time that elapsed; it
        // is not derived from the instruction count any more.
        assert_eq!(accounting.device_cck, run.bus_advanced_cck);
        assert_eq!(accounting.slice_cck, run.bus_advanced_cck);
    }

    #[test]
    fn running_real_slice_device_time_is_the_elapsed_bus_time() {
        // Cycle-exact core: there is no post-hoc reconciliation between reported
        // CPU cycles and chip-bus accesses, so a running slice carries no extra
        // "chip-bus wait" and its device/slice time both equal the bus time that
        // already elapsed during execution (sync clocks + bus-cycle tails +
        // grants + contention waits). chip_bus_wait_cck is always 0.
        let run = ExecutedSlice {
            actual_instructions: 10,
            actual_cpu_cycles: 70,
            actual_cpu_cck: 35,
            bus_advanced_cck: 64,
            cpu_stopped: false,
        };

        let accounting = real_slice_accounting(
            &run,
            4096,
            DEFAULT_CPU_CYCLES_PER_INSTRUCTION,
            RealPacingBudgetMode::RetiredInstructions,
        );
        assert_eq!(accounting.device_cck, 64);
        assert_eq!(accounting.slice_cck, 64);
        assert_eq!(accounting.chip_bus_wait_cck, 0);
    }

    #[test]
    fn cycle_budget_mode_debits_instructions_for_elapsed_bus_time() {
        // M68kCycles mode now debits the budget by the instruction-equivalent of
        // the device time, and for a running slice the device time IS the bus
        // time that elapsed (bus_advanced_cck). There is no separate chip-bus
        // wait term to add: budget = instructions_for_cck_value(bus_advanced_cck).
        let run = ExecutedSlice {
            actual_instructions: 10,
            actual_cpu_cycles: 70,
            actual_cpu_cck: 35,
            bus_advanced_cck: 14,
            cpu_stopped: false,
        };

        let accounting = real_slice_accounting(
            &run,
            4096,
            DEFAULT_CPU_CYCLES_PER_INSTRUCTION,
            RealPacingBudgetMode::M68kCycles,
        );

        assert_eq!(accounting.device_cck, 14);
        assert_eq!(
            accounting.budget_debit,
            instructions_for_cck_value(14, DEFAULT_CPU_CYCLES_PER_INSTRUCTION)
        );
    }

    #[test]
    fn cycle_budget_debits_more_than_instruction_budget_for_expensive_instructions() {
        // Regression for blitter-bound vector scenes: the main loop runs from
        // chip RAM and the vectors are drawn with the blitter. That instruction
        // mix really costs more
        // device (bus) time than the flat 4.0 cycles/instruction the
        // instruction budget assumes: here 10 instructions consumed 70 cck of
        // bus time (7 cck = 14 CPU clocks each) of chip accesses, tails and
        // contention waits. With instruction pacing the CPU is clocked at the
        // flat rate and retires too many instructions per frame, issuing
        // chip-bus cycles faster than hardware and starving the very blitter it
        // waits on. Cycle pacing debits the budget by the real elapsed device
        // time, so it must charge at least as much as -- and for above-flat-cost
        // code strictly more than -- instruction pacing.
        let run = ExecutedSlice {
            actual_instructions: 10,
            actual_cpu_cycles: 70,
            actual_cpu_cck: 35,
            bus_advanced_cck: 70,
            cpu_stopped: false,
        };

        let instr = real_slice_accounting(
            &run,
            4096,
            DEFAULT_CPU_CYCLES_PER_INSTRUCTION,
            RealPacingBudgetMode::RetiredInstructions,
        );
        let cycles = real_slice_accounting(
            &run,
            4096,
            DEFAULT_CPU_CYCLES_PER_INSTRUCTION,
            RealPacingBudgetMode::M68kCycles,
        );

        assert_eq!(instr.budget_debit, 10);
        assert_eq!(
            cycles.budget_debit,
            instructions_for_cck_value(70, DEFAULT_CPU_CYCLES_PER_INSTRUCTION)
        );
        assert!(
            cycles.budget_debit > instr.budget_debit,
            "cycle pacing ({}) must debit more than instruction pacing ({}) for \
             above-flat-cost instructions",
            cycles.budget_debit,
            instr.budget_debit
        );
    }

    #[test]
    fn parse_pacing_budget_env_recognizes_known_modes_and_ignores_others() {
        // Absent or unrecognized: None, so the caller's config default stands.
        assert_eq!(super::parse_real_pacing_budget_mode(None), None);
        assert_eq!(super::parse_real_pacing_budget_mode(Some("bogus")), None);
        assert_eq!(
            super::parse_real_pacing_budget_mode(Some("instructions")),
            Some(RealPacingBudgetMode::RetiredInstructions)
        );
        assert_eq!(
            super::parse_real_pacing_budget_mode(Some("cycles")),
            Some(RealPacingBudgetMode::M68kCycles)
        );
    }

    #[test]
    fn pacing_budget_config_maps_to_pacing_mode() {
        assert_eq!(
            RealPacingBudgetMode::from(PacingBudget::Cycles),
            RealPacingBudgetMode::M68kCycles
        );
        assert_eq!(
            RealPacingBudgetMode::from(PacingBudget::Instructions),
            RealPacingBudgetMode::RetiredInstructions
        );
    }

    #[test]
    fn real_pacing_profile_accumulates_slice_sleep_and_audio_state() {
        let run = ExecutedSlice {
            actual_instructions: 12,
            actual_cpu_cycles: 84,
            actual_cpu_cck: 42,
            bus_advanced_cck: 50,
            cpu_stopped: false,
        };
        let accounting = real_slice_accounting(
            &run,
            4096,
            DEFAULT_CPU_CYCLES_PER_INSTRUCTION,
            RealPacingBudgetMode::RetiredInstructions,
        );
        let mut profile = RealPacingProfile::enabled_for_test();

        profile.record_slice(&run, accounting);
        profile.record_sleep(Duration::from_millis(2));
        profile.record_wall_overrun(Duration::from_millis(3));

        assert_eq!(profile.retired_instructions, 12);
        assert_eq!(profile.m68k_cycles, 84);
        assert_eq!(
            profile.chip_bus_wait_cck,
            u64::from(accounting.chip_bus_wait_cck)
        );
        assert_eq!(profile.device_cck, u64::from(accounting.device_cck));
        assert_eq!(profile.sleep_count, 1);
        assert_eq!(profile.sleep_nanos, Duration::from_millis(2).as_nanos());
        assert_eq!(profile.wall_overrun_count, 1);
        assert_eq!(
            profile.wall_overrun_nanos,
            Duration::from_millis(3).as_nanos()
        );

        profile.last_log = Instant::now() - Duration::from_secs(2);
        profile.log_if_due(
            AudioRuntimeStatus {
                queue_depth_frames: 64,
                output_lead_seconds: 0.01,
                callback_underrun_frames: 2,
                dropped_overrun_frames: 3,
                skipped_stale_frames: 4,
            },
            0,
        );
        assert_eq!(profile.retired_instructions, 0);
        assert_eq!(profile.m68k_cycles, 0);
        assert_eq!(profile.chip_bus_wait_cck, 0);
        assert_eq!(profile.device_cck, 0);
        assert_eq!(profile.sleep_count, 0);
        assert_eq!(profile.sleep_nanos, 0);
        assert_eq!(profile.wall_overrun_count, 0);
        assert_eq!(profile.wall_overrun_nanos, 0);
    }

    #[test]
    fn config_speed_maps_to_the_single_real_timing() {
        assert_eq!(
            RuntimeTiming::from_config(SpeedMode::Real, true),
            RuntimeTiming::Real
        );
        assert_eq!(
            RuntimeTiming::from_config(SpeedMode::Real, false),
            RuntimeTiming::Real
        );
    }

    #[test]
    fn real_mode_uses_stock_realtime_budget() {
        std::env::remove_var("COPPERLINE_REAL_CPU_CPI");
        let cpu_cycles_per_instruction = super::real_cpu_cycles_per_instruction();
        let target = super::real_target_instructions_per_second(cpu_cycles_per_instruction);
        assert_eq!(
            RuntimeTiming::Real.realtime_budget(cpu_cycles_per_instruction),
            ((target / 60.0).round() as usize, target)
        );
    }

    #[test]
    fn real_mode_waits_when_device_time_runs_ahead_of_wall_time() {
        let started_at = Instant::now();
        let now = started_at + Duration::from_millis(900);

        assert_eq!(
            super::realtime_device_time_wait(started_at, now, 1.0, 0.0),
            Some(Duration::from_millis(100))
        );
        assert_eq!(
            super::realtime_device_time_wait(started_at, now, 0.5, 0.0),
            None
        );
    }

    #[test]
    fn real_mode_preserves_live_audio_output_lead() {
        let started_at = Instant::now();
        let now = started_at + Duration::from_millis(900);

        assert_eq!(
            super::realtime_device_time_wait(started_at, now, 1.0, 0.2),
            None
        );
        assert_eq!(
            super::realtime_device_time_wait(started_at, now, 1.2, 0.2),
            Some(Duration::from_millis(100))
        );
    }
}
