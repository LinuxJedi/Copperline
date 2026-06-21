// SPDX-License-Identifier: GPL-3.0-or-later

//! Top-level emulator: owns the M68K CPU instance and drives execution
//! in fixed-size instruction slices, advancing the raster after each
//! slice and raising chipset/CIA/Paula interrupts.

use crate::audio::{audio_profile_enabled, AudioRuntimeStatus};
use crate::bus::Bus;
use crate::config::{CpuModel, PacingBudget};
use crate::cpu;
use anyhow::Result;
use std::time::{Duration, Instant};

const INSTRUCTIONS_PER_SLICE: usize = 32_000;
const INSTRUCTIONS_PER_REALTIME_SLICE: usize = 8_192;
/// Safety bound on a single reverse-debug replay so a pathological target
/// (e.g. a permanently halted CPU) cannot spin forever. Far larger than the
/// instruction distance between two snapshots at any sane capture interval.
const TT_REPLAY_STEP_CAP: u64 = 100_000_000;
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
    /// When true, pace presentation to wall-clock time (interactive
    /// window). When false, run the deterministic core unthrottled
    /// (headless screenshot/frame-dump runs). The emulated result is
    /// identical either way.
    paced: bool,
    cpu_cycles_per_instruction: f64,
    real_pacing_budget_mode: RealPacingBudgetMode,
    audio_profile: AudioRuntimeProfile,
    real_pacing_profile: RealPacingProfile,
    /// Monotonic count of retired CPU instructions since power-on -- the
    /// position coordinate for reverse debugging. Kept outside the
    /// serialized machine state so capturing it is free and the save-state
    /// format is unaffected.
    retired_instructions: u64,
    /// Reverse-debug snapshot ring, present only when reverse mode is armed.
    tt_ring: Option<crate::timetravel::SnapshotRing>,
    /// Position-keyed log of applied input actions, recorded during the
    /// forward run and re-applied during reverse replay. Present whenever the
    /// ring is.
    tt_input: Option<crate::inputsched::ReplayInputLog>,
    /// One-shot "last writer" reverse watchpoint (`COPPERLINE_DBG_RWATCH`).
    tt_rwatch: Option<ReverseWatch>,
    /// Shape of the running machine, stamped into save states and compared
    /// against a loaded state's stamp so a mismatch can reconfigure the host
    /// to match the state. Set from the boot `Config`; updated on a load that
    /// swaps in a different machine.
    descriptor: crate::config::MachineDescriptor,
}

/// What a save-state load did, for the caller to surface. A `.clstate` always
/// rebuilds its own machine, so `reconfigured` reports whether that machine
/// differed from the one that was running (host pacing was re-derived either
/// way).
pub struct StateLoadOutcome {
    /// True when the loaded state's machine shape differed from the running
    /// machine, so the host was reconfigured to match the state.
    pub reconfigured: bool,
    /// One-line human summary of the loaded machine.
    pub summary: String,
}

/// A one-shot headless reverse watchpoint: at `target_secs` (or run end),
/// report the last instruction that wrote `addr`, then disarm.
struct ReverseWatch {
    addr: u32,
    target_secs: Option<f64>,
    fired: bool,
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

/// Per-frame instruction quantum and the target instructions/second for the
/// deterministic real-time core. CPU and chipset/audio advance together in
/// emulated time; presentation is wall-clock paced for the window and
/// unthrottled for headless runs, but the emulated result is identical.
fn realtime_budget(cpu_cycles_per_instruction: f64) -> (usize, f64) {
    let target = real_target_instructions_per_second(cpu_cycles_per_instruction);
    ((target / 60.0).round().max(1.0) as usize, target)
}

fn real_cpu_cycles_per_instruction() -> f64 {
    crate::envcfg::var("COPPERLINE_REAL_CPU_CPI")
        .and_then(|raw| raw.parse::<f64>().ok())
        .filter(|value| value.is_finite() && *value > 0.0)
        .unwrap_or(DEFAULT_CPU_CYCLES_PER_INSTRUCTION)
}

/// Host pacing cost per retired instruction for a CPU clocked at
/// `cpu_clocks_per_cck` clocks per colour clock. The pacing math is expressed
/// against the stock 2-clocks-per-CCK 68000 ratio; an accelerated CPU retires
/// instructions faster relative to the chipset, which is equivalent to folding
/// `CPU_CYCLES_PER_COLOR_CLOCK / cpu_clocks_per_cck` into the per-instruction
/// cost (an identity at the stock ratio of 2). Computed in `Emulator::new` and
/// recomputed after a save-state load that swaps in a differently-clocked CPU.
fn cpu_cycles_per_instruction_for_clock(cpu_clocks_per_cck: u32) -> f64 {
    let speed_factor = CPU_CYCLES_PER_COLOR_CLOCK / cpu_clocks_per_cck.max(1) as f64;
    real_cpu_cycles_per_instruction() * speed_factor
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
        pacing_budget: PacingBudget,
        cpu_clocks_per_cck: u32,
        paced: bool,
    ) -> Result<Self> {
        let cpu_clocks_per_cck = cpu_clocks_per_cck.max(1);
        // Fold the CPU speed multiple into the effective cycles-per-instruction
        // so the pacing helpers stay expressed against the stock 68000 ratio
        // (see `cpu_cycles_per_instruction_for_clock`).
        let cpu_cycles_per_instruction = cpu_cycles_per_instruction_for_clock(cpu_clocks_per_cck);
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
            paced,
            cpu_cycles_per_instruction,
            real_pacing_budget_mode,
            audio_profile: AudioRuntimeProfile::new(),
            real_pacing_profile: RealPacingProfile::new(),
            retired_instructions: 0,
            tt_ring: None,
            tt_input: None,
            tt_rwatch: None,
            descriptor: crate::config::MachineDescriptor::default(),
        })
    }

    /// Install the opt-in 68020/030 CACR-controlled cache models.
    pub fn set_cache_emulation(&mut self, icache: bool, dcache: bool) {
        self.machine.set_cache_emulation(icache, dcache);
    }

    /// Record the shape of the running machine (from the boot `Config`) and
    /// fingerprint its in-memory ROM. The descriptor is stamped into save
    /// states and compared against a loaded state's stamp.
    pub fn set_machine_descriptor(&mut self, descriptor: crate::config::MachineDescriptor) {
        self.descriptor = descriptor;
        self.refresh_rom_fingerprint();
    }

    /// Re-fingerprint the descriptor's ROM from the live in-memory images.
    /// Call whenever the shape descriptor is (re)set or the ROM is swapped.
    fn refresh_rom_fingerprint(&mut self) {
        let mem = &self.machine.bus().mem;
        self.descriptor
            .set_rom_fingerprint(&mem.rom, &mem.extended_rom);
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

    /// Discard live-output samples queued for an emulated timeline that has
    /// just been abandoned. This is host presentation state only; Paula's
    /// serialized DMA/mixer state is left untouched.
    pub fn reset_live_audio_after_timeline_jump(&mut self) {
        self.bus_mut().reset_live_audio_after_timeline_jump();
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
        // Keep the machine descriptor's ROM fingerprint in step with the
        // freshly fitted ROM, so a state saved after a swap stamps the new ROM.
        self.refresh_rom_fingerprint();
        log::info!("boot ROM replaced; cold-resetting");
        self.power_on_reset()
    }

    /// Write a save state of the whole emulated machine to `path`. Call
    /// between frames (the event loop and the headless frame loop both run
    /// at frame granularity, so any caller outside step_frame qualifies).
    pub fn save_state(&self, path: &std::path::Path) -> Result<()> {
        crate::savestate::save(&self.machine, &self.descriptor, path)
    }

    /// Restore a save state from `path`. The state carries its own machine
    /// (RAM, ROM, chip revisions, CPU), so a load fully rebuilds it; when that
    /// machine differs from the one running, the host is reconfigured to match
    /// the state (the descriptor is adopted and pacing re-derived) and the
    /// difference is logged. On success emulated time jumps to the state's
    /// timeline, so the real-time pacing anchor is re-baselined to "now"; on
    /// failure the running machine is untouched.
    pub fn load_state(&mut self, path: &std::path::Path) -> Result<StateLoadOutcome> {
        let loaded = crate::savestate::load(&mut self.machine, path)?;
        let reconfigured = loaded != self.descriptor;
        if reconfigured {
            let diffs = self.descriptor.differences(&loaded).join(", ");
            log::warn!(
                "save state describes a different machine than the running config \
                 ({diffs}); reconfiguring host to match the state ({})",
                loaded.summary()
            );
            self.descriptor = loaded.clone();
        }
        // The CPU clock travels with the state; re-derive the host pacing math
        // from it so an accelerated/slower restored CPU is paced correctly.
        self.reconfigure_pacing_for_cpu_clock();
        self.reset_live_audio_after_timeline_jump();
        self.reanchor_realtime_clock();
        Ok(StateLoadOutcome {
            reconfigured,
            summary: loaded.summary(),
        })
    }

    /// Re-derive the host pacing cost-per-instruction from the machine's
    /// current CPU-clocks-per-colour-clock. `Emulator::new` computes this once
    /// from the boot config; a save-state load can swap in a CPU with a
    /// different clock, so recompute it then. See `new` for the derivation.
    fn reconfigure_pacing_for_cpu_clock(&mut self) {
        self.cpu_cycles_per_instruction =
            cpu_cycles_per_instruction_for_clock(self.machine.cpu_clocks_per_cck());
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

    // ---- Reverse debugging (time travel) ------------------------------

    /// Monotonic count of retired CPU instructions since power-on -- the
    /// position coordinate reverse-debug ops navigate by.
    pub fn retired_instructions(&self) -> u64 {
        self.retired_instructions
    }

    /// Arm the reverse-debug snapshot ring (replacing any existing ring).
    /// Captures begin at the next frame boundary; `budget_mb` caps total
    /// snapshot memory and `interval_frames` is the gap between captures.
    pub fn enable_time_travel(&mut self, budget_mb: usize, interval_frames: u64) {
        self.tt_ring = Some(crate::timetravel::SnapshotRing::new(
            budget_mb,
            interval_frames,
        ));
        self.tt_input = Some(crate::inputsched::ReplayInputLog::new());
    }

    pub fn time_travel_enabled(&self) -> bool {
        self.tt_ring.is_some()
    }

    pub fn time_travel_ring(&self) -> Option<&crate::timetravel::SnapshotRing> {
        self.tt_ring.as_ref()
    }

    /// Record an input action at the current position for deterministic
    /// reverse replay. No-op unless reverse mode is armed; the live forward
    /// application is unchanged and still done by the caller.
    pub fn tt_note_input(&mut self, action: crate::inputsched::ReplayAction) {
        let pos = self.retired_instructions;
        if let Some(log) = self.tt_input.as_mut() {
            log.record(pos, action);
        }
    }

    /// Position the replay-input cursor for a replay starting at `from_pos`.
    fn tt_begin_replay_input(&mut self, from_pos: u64) {
        if let Some(log) = self.tt_input.as_mut() {
            log.begin_replay(from_pos);
        }
    }

    /// Apply any input actions that come due at or before `pos` during replay.
    fn tt_apply_due_input(&mut self, pos: u64) {
        let mut due = Vec::new();
        if let Some(log) = self.tt_input.as_mut() {
            log.take_due(pos, &mut due);
        }
        for action in due {
            action.apply(self.bus_mut());
        }
    }

    /// Serialize the whole machine into an in-memory blob (bincode only, no
    /// zlib/magic framing -- snapshots are same-process and need no format
    /// versioning, see `timetravel`).
    fn snapshot_blob(&self) -> Result<Vec<u8>> {
        let mut buf = Vec::new();
        self.machine.write_state(&mut buf)?;
        Ok(buf)
    }

    /// Restore a blob produced by `snapshot_blob` and rebase the position
    /// coordinate to `pos`. The pacing anchor is re-baselined like a normal
    /// save-state load.
    fn restore_blob(&mut self, blob: &[u8], pos: u64) -> Result<()> {
        let mut cursor = std::io::Cursor::new(blob);
        self.machine.apply_state(&mut cursor)?;
        self.retired_instructions = pos;
        self.reset_live_audio_after_timeline_jump();
        self.reanchor_realtime_clock();
        Ok(())
    }

    /// Capture a snapshot into the ring if one is due at the current frame.
    fn tt_capture_if_due(&mut self) -> Result<()> {
        let frame = self.bus().emulated_frames();
        let due = match self.tt_ring.as_ref() {
            Some(ring) => ring.capture_due(frame),
            None => return Ok(()),
        };
        if !due {
            return Ok(());
        }
        let pos = self.retired_instructions;
        let blob = self.snapshot_blob()?;
        if let Some(ring) = self.tt_ring.as_mut() {
            ring.push(crate::timetravel::Snapshot { pos, frame, blob });
        }
        // Drop input-log entries older than the oldest retained snapshot: they
        // can never be replayed again.
        if let Some(oldest) = self.tt_ring.as_ref().and_then(|r| r.oldest_pos()) {
            if let Some(log) = self.tt_input.as_mut() {
                log.prune_before(oldest);
            }
        }
        Ok(())
    }

    /// Replay forward from the current state up to instruction position
    /// `target_pos`, single-stepping faithfully (the same `run_one_step` the
    /// forward run uses). Stops early if the CPU deadlocks (halted with no
    /// pending wake-up) or the safety step cap is hit.
    fn tt_replay_to(&mut self, target_pos: u64) -> Result<()> {
        // Re-apply any input recorded at the anchor position before stepping.
        self.tt_apply_due_input(self.retired_instructions);
        let mut cpu_idle = false;
        let mut guard: u64 = 0;
        while self.retired_instructions < target_pos {
            let prev = self.retired_instructions;
            self.run_one_step(&mut cpu_idle, INSTRUCTIONS_PER_SLICE)?;
            self.tt_apply_due_input(self.retired_instructions);
            // No forward progress and not merely idling toward a wake-up means
            // a permanent halt; bail rather than spin forever.
            if self.retired_instructions == prev && !cpu_idle {
                break;
            }
            guard += 1;
            if guard > TT_REPLAY_STEP_CAP {
                log::warn!(
                    "reverse-debug replay hit the {TT_REPLAY_STEP_CAP}-step cap before reaching pos {target_pos}"
                );
                break;
            }
        }
        Ok(())
    }

    /// Reconstruct the machine exactly at instruction position `target_pos`
    /// by restoring the nearest earlier snapshot and replaying forward. The
    /// ring is left intact (reverse ops never capture).
    pub fn tt_restore_to(
        &mut self,
        target_pos: u64,
    ) -> Result<crate::timetravel::ReverseOutcome<()>> {
        use crate::timetravel::ReverseOutcome;
        let anchor = match self
            .tt_ring
            .as_ref()
            .and_then(|r| r.nearest_at_or_before(target_pos))
        {
            Some(s) => (s.pos, s.blob.clone()),
            None => return Ok(ReverseOutcome::BeyondHistory),
        };
        self.restore_blob(&anchor.1, anchor.0)?;
        self.tt_begin_replay_input(anchor.0);
        self.tt_replay_to(target_pos)?;
        Ok(ReverseOutcome::Found(()))
    }

    /// Step backward `n` instructions. On success the machine is left exactly
    /// at the new (earlier) position, returned in `Found`.
    pub fn tt_reverse_step(&mut self, n: u64) -> Result<crate::timetravel::ReverseOutcome<u64>> {
        use crate::timetravel::ReverseOutcome;
        let target = self.retired_instructions.saturating_sub(n);
        Ok(match self.tt_restore_to(target)? {
            ReverseOutcome::Found(()) => ReverseOutcome::Found(self.retired_instructions),
            ReverseOutcome::NotFound => ReverseOutcome::NotFound,
            ReverseOutcome::BeyondHistory => ReverseOutcome::BeyondHistory,
        })
    }

    /// Step backward to the first instruction boundary in the previous
    /// emulated video frame. The target is the Agnus frame counter crossing,
    /// not a host scheduler quantum.
    pub fn tt_reverse_frame(&mut self) -> Result<crate::timetravel::ReverseOutcome<u64>> {
        use crate::timetravel::ReverseOutcome;
        let current_frame = self.bus().emulated_frames();
        let Some(target_frame) = current_frame.checked_sub(1) else {
            return Ok(ReverseOutcome::NotFound);
        };
        let saved_pos = self.retired_instructions;
        let saved_blob = self.snapshot_blob()?;
        let mut interval_end = self.retired_instructions;
        let outcome = loop {
            let anchor = match self
                .tt_ring
                .as_ref()
                .and_then(|r| r.nearest_before(interval_end))
            {
                Some(s) => (s.pos, s.frame, s.blob.clone()),
                None => break ReverseOutcome::BeyondHistory,
            };
            let anchor_is_oldest =
                self.tt_ring.as_ref().and_then(|r| r.oldest_pos()) == Some(anchor.0);
            self.restore_blob(&anchor.2, anchor.0)?;
            self.tt_begin_replay_input(anchor.0);
            if target_frame == 0 && anchor.0 == 0 && anchor.1 == 0 {
                break ReverseOutcome::Found(0);
            }
            if anchor.1 < target_frame {
                if let Some(pos) = self.tt_scan_frame_start(target_frame, interval_end)? {
                    self.tt_restore_to(pos)?;
                    break ReverseOutcome::Found(pos);
                }
            }
            if anchor_is_oldest {
                break ReverseOutcome::BeyondHistory;
            }
            interval_end = anchor.0;
        };
        if !matches!(outcome, ReverseOutcome::Found(_)) {
            self.restore_blob(&saved_blob, saved_pos)?;
        }
        Ok(outcome)
    }

    /// Run backward to the previous interactive breakpoint hit: the latest
    /// instruction boundary strictly before the current position whose PC is
    /// an armed breakpoint. On `Found` the machine is left parked there.
    /// `NotFound` means no breakpoints are set or none fired in retained
    /// history that starts at power-on; `BeyondHistory` means an earlier hit
    /// may exist before the oldest snapshot. (Watch-based reverse-continue is
    /// not yet modelled; breakpoints only.)
    pub fn tt_reverse_continue(&mut self) -> Result<crate::timetravel::ReverseOutcome<u64>> {
        use crate::timetravel::ReverseOutcome;
        let breakpoints = self.machine.ui_breaks().breakpoints.clone();
        if breakpoints.is_empty() {
            return Ok(ReverseOutcome::NotFound);
        }
        let mut interval_end = self.retired_instructions;
        loop {
            let anchor = match self
                .tt_ring
                .as_ref()
                .and_then(|r| r.nearest_before(interval_end))
            {
                Some(s) => (s.pos, s.blob.clone()),
                None => return Ok(ReverseOutcome::BeyondHistory),
            };
            let anchor_is_oldest =
                self.tt_ring.as_ref().and_then(|r| r.oldest_pos()) == Some(anchor.0);
            self.restore_blob(&anchor.1, anchor.0)?;
            self.tt_begin_replay_input(anchor.0);
            if let Some(pos) = self.tt_scan_breakpoint(&breakpoints, interval_end)? {
                self.tt_restore_to(pos)?;
                return Ok(ReverseOutcome::Found(pos));
            }
            if anchor_is_oldest {
                return Ok(if anchor.0 == 0 {
                    ReverseOutcome::NotFound
                } else {
                    ReverseOutcome::BeyondHistory
                });
            }
            interval_end = anchor.0;
        }
    }

    /// Replay the just-restored interval up to `end_pos`, returning the latest
    /// boundary (strictly before `end_pos`) whose PC is an armed breakpoint --
    /// the "about to execute a breakpoint" stop the forward run uses.
    fn tt_scan_breakpoint(&mut self, breakpoints: &[u32], end_pos: u64) -> Result<Option<u64>> {
        const PC_MASK: u32 = 0x00FF_FFFF;
        let is_bp = |pc: u32| breakpoints.contains(&(pc & PC_MASK));
        self.tt_apply_due_input(self.retired_instructions);
        let mut best = None;
        if self.retired_instructions < end_pos && is_bp(self.machine.pc()) {
            best = Some(self.retired_instructions);
        }
        let mut cpu_idle = false;
        let mut guard: u64 = 0;
        while self.retired_instructions < end_pos {
            let before = self.retired_instructions;
            self.run_one_step(&mut cpu_idle, INSTRUCTIONS_PER_SLICE)?;
            self.tt_apply_due_input(self.retired_instructions);
            if self.retired_instructions < end_pos && is_bp(self.machine.pc()) {
                best = Some(self.retired_instructions);
            }
            if self.retired_instructions == before && !cpu_idle {
                break;
            }
            guard += 1;
            if guard > TT_REPLAY_STEP_CAP {
                break;
            }
        }
        Ok(best)
    }

    /// Replay the just-restored interval up to `end_pos`, returning the first
    /// instruction boundary whose Agnus frame counter has reached
    /// `target_frame`.
    fn tt_scan_frame_start(&mut self, target_frame: u64, end_pos: u64) -> Result<Option<u64>> {
        self.tt_apply_due_input(self.retired_instructions);
        if self.machine.bus().emulated_frames() >= target_frame {
            return Ok(Some(self.retired_instructions));
        }
        let mut cpu_idle = false;
        let mut guard: u64 = 0;
        while self.retired_instructions < end_pos {
            let before = self.retired_instructions;
            self.run_one_step(&mut cpu_idle, INSTRUCTIONS_PER_SLICE)?;
            self.tt_apply_due_input(self.retired_instructions);
            if self.machine.bus().emulated_frames() >= target_frame {
                return Ok(Some(self.retired_instructions));
            }
            if self.retired_instructions == before && !cpu_idle {
                break;
            }
            guard += 1;
            if guard > TT_REPLAY_STEP_CAP {
                break;
            }
        }
        Ok(None)
    }

    /// Find the last instruction before position `before_pos` that changed the
    /// word at `addr`. Walks snapshot intervals backward, replaying each with
    /// a watch on `addr`, until a change is found or retained history runs
    /// out. On `Found` the machine is repositioned exactly at the writing
    /// instruction so the caller can inspect it.
    pub fn tt_last_writer(
        &mut self,
        addr: u32,
        before_pos: u64,
    ) -> Result<crate::timetravel::ReverseOutcome<crate::timetravel::WriteRecord>> {
        use crate::timetravel::ReverseOutcome;
        let mut interval_end = before_pos;
        loop {
            let anchor = match self
                .tt_ring
                .as_ref()
                .and_then(|r| r.nearest_before(interval_end))
            {
                Some(s) => (s.pos, s.blob.clone()),
                None => return Ok(ReverseOutcome::BeyondHistory),
            };
            let anchor_is_oldest =
                self.tt_ring.as_ref().and_then(|r| r.oldest_pos()) == Some(anchor.0);
            self.restore_blob(&anchor.1, anchor.0)?;
            self.tt_begin_replay_input(anchor.0);
            if let Some(rec) = self.tt_scan_writes(addr, interval_end)? {
                // Leave the machine parked on the writing instruction.
                self.tt_restore_to(rec.pos)?;
                return Ok(ReverseOutcome::Found(rec));
            }
            // Nothing in this interval; step one interval further back.
            if anchor_is_oldest {
                // We scanned the oldest retained interval. If it starts at
                // power-on the answer is a definitive "never written";
                // otherwise an earlier write may exist beyond history.
                return Ok(if anchor.0 == 0 {
                    ReverseOutcome::NotFound
                } else {
                    ReverseOutcome::BeyondHistory
                });
            }
            interval_end = anchor.0;
        }
    }

    /// Replay from the current (just-restored) state to `end_pos`, returning
    /// the last change to the word at `addr` seen along the way. The writer
    /// PC is the previous-instruction PC, matching the forward
    /// `COPPERLINE_DBG_WATCH` attribution.
    fn tt_scan_writes(
        &mut self,
        addr: u32,
        end_pos: u64,
    ) -> Result<Option<crate::timetravel::WriteRecord>> {
        // Apply any input recorded at the anchor before observing writes.
        self.tt_apply_due_input(self.retired_instructions);
        let mut last: Option<crate::timetravel::WriteRecord> = None;
        let mut prev = self.machine.bus().peek_word_any(addr);
        let mut cpu_idle = false;
        let mut guard: u64 = 0;
        while self.retired_instructions < end_pos {
            let before = self.retired_instructions;
            self.run_one_step(&mut cpu_idle, INSTRUCTIONS_PER_SLICE)?;
            self.tt_apply_due_input(self.retired_instructions);
            let cur = self.machine.bus().peek_word_any(addr);
            if cur != prev {
                last = Some(crate::timetravel::WriteRecord {
                    addr,
                    old: prev,
                    new: cur,
                    pc: self.machine.ppc(),
                    pos: self.retired_instructions,
                    cck: self.machine.bus().emulated_cck(),
                    frame: self.machine.bus().emulated_frames(),
                });
                prev = cur;
            }
            if self.retired_instructions == before && !cpu_idle {
                break;
            }
            guard += 1;
            if guard > TT_REPLAY_STEP_CAP {
                break;
            }
        }
        Ok(last)
    }

    /// Arm a one-shot "last writer" reverse watchpoint on `addr`, evaluated at
    /// `target_secs` of emulated time (or at run end via
    /// `tt_finalize_reverse_watch` when `None`). Requires the ring to be armed.
    pub fn arm_reverse_watch(&mut self, addr: u32, target_secs: Option<f64>) {
        self.tt_rwatch = Some(ReverseWatch {
            addr,
            target_secs,
            fired: false,
        });
    }

    fn tt_poll_reverse_watch(&mut self) -> Result<()> {
        let due = match self.tt_rwatch.as_ref() {
            Some(rw) if !rw.fired => match rw.target_secs {
                Some(t) => self.bus().emulated_seconds() >= t,
                None => false, // run-end target: see tt_finalize_reverse_watch
            },
            _ => false,
        };
        if due {
            self.tt_fire_reverse_watch()?;
        }
        Ok(())
    }

    /// Evaluate a pending reverse watchpoint now (used at run end for an
    /// untargeted `COPPERLINE_DBG_RWATCH`, and as a safety net if the target
    /// time was never reached). Idempotent.
    pub fn tt_finalize_reverse_watch(&mut self) -> Result<()> {
        self.tt_fire_reverse_watch()
    }

    /// Run the reverse "last writer" query for the armed watchpoint and report
    /// it, preserving the live forward state across the (state-mutating)
    /// query so the run continues unaffected.
    fn tt_fire_reverse_watch(&mut self) -> Result<()> {
        use crate::timetravel::ReverseOutcome;
        let addr = match self.tt_rwatch.as_ref() {
            Some(rw) if !rw.fired => rw.addr,
            _ => return Ok(()),
        };
        if let Some(rw) = self.tt_rwatch.as_mut() {
            rw.fired = true;
        }
        let pos_now = self.retired_instructions;
        // Snapshot the live state, run the backward query, then restore so the
        // forward run resumes exactly where it left off.
        let saved = self.snapshot_blob()?;
        let outcome = self.tt_last_writer(addr, pos_now)?;
        match outcome {
            ReverseOutcome::Found(rec) => log::info!(
                "DBG RWATCH last writer of ${:06X}: {:04X}->{:04X} by pc={:#010X} pos={} f={} cck={}",
                rec.addr,
                rec.old,
                rec.new,
                rec.pc,
                rec.pos,
                rec.frame,
                rec.cck,
            ),
            ReverseOutcome::NotFound => log::info!(
                "DBG RWATCH ${addr:06X}: no write to it found in recorded history"
            ),
            ReverseOutcome::BeyondHistory => log::warn!(
                "DBG RWATCH ${addr:06X}: the last write predates retained snapshots; \
                 raise COPPERLINE_DBG_RR_BUDGET_MB or lower COPPERLINE_DBG_RR_INTERVAL"
            ),
        }
        self.restore_blob(&saved, pos_now)?;
        Ok(())
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

    pub fn reset_stats(&mut self) {
        self.stats = EmuStats::default();
        self.bus_mut().reset_profile_stats();
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
        self.step_real()?;
        self.stats.frames += 1;
        // Capture a reverse-debug snapshot at this frame boundary when one is
        // due (no-op unless reverse mode is armed). Frame boundaries are the
        // only safe capture points -- mid-frame the renderer capture buffers
        // are inconsistent (see M68kMachine::write_state).
        self.tt_capture_if_due()?;
        // Evaluate a time-targeted reverse watchpoint when its target is
        // reached (no-op unless armed).
        self.tt_poll_reverse_watch()?;
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
            let accounting = self.run_one_step(&mut cpu_idle, remaining)?;
            remaining = remaining.saturating_sub(accounting.budget_debit);
            // An interactive breakpoint/watch hit ends the frame early;
            // the window surfaces it and pauses. (Checked after the device
            // advance so a hit during an idle fast-forward is seen too.)
            if self.machine.ui_debug_stop_pending() {
                break;
            }
        }
        // Pace presentation to wall-clock only for the interactive window;
        // headless runs advance the deterministic core unthrottled.
        if self.paced {
            self.sleep_until_realtime_device_time();
        }
        Ok(())
    }

    /// One iteration of the cycle-stepping loop: pick a slice size (a single
    /// instruction while running, or an idle fast-forward bounded by
    /// `idle_cap` while the CPU is halted in STOP), execute it, advance idle
    /// device time, and recognize interrupts. `cpu_idle` carries the
    /// STOP-state flag across calls. Returns the pacing accounting for the
    /// slice. Shared by `step_real` (budget-driven) and the reverse-debug
    /// replay loop (position-driven), so replay reproduces the forward run
    /// instruction-for-instruction.
    fn run_one_step(
        &mut self,
        cpu_idle: &mut bool,
        idle_cap: usize,
    ) -> Result<RealSliceAccounting> {
        let chunk = if *cpu_idle {
            self.idle_fast_forward_chunk(idle_cap)
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
        if run.cpu_stopped {
            // A stopped CPU performed no bus activity; advance the chipset
            // and timed devices through the idle period. (A running slice
            // needs nothing here: the cycle-exact core already advanced
            // its full device time through sync/grant as it executed.)
            let idle_cck = accounting.slice_cck.saturating_sub(run.bus_advanced_cck);
            self.bus_mut().advance_devices(idle_cck);
        }
        // `refresh_irq_line` applies any deferred timed-device color clocks
        // before sampling the interrupt line (see its body), so a device
        // interrupt that came due during the slice is recognized here.
        self.machine.refresh_irq_line();
        self.real_pacing_profile.record_slice(&run, accounting);
        // Only a single-instruction slice that came back stopped tells us the
        // CPU is genuinely idle; never batch on the slice right after a
        // fast-forward, so a wake-up is always stepped.
        *cpu_idle = run.cpu_stopped && chunk == 1;
        Ok(accounting)
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
        let (quantum, _target) = realtime_budget(self.cpu_cycles_per_instruction);
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
        let live_audio_lead_seconds = self.bus().live_audio_output_lead_seconds();
        let target = realtime_device_time_target(
            started_at,
            self.bus().emulated_seconds(),
            live_audio_lead_seconds,
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
                    if lag > realtime_catchup_limit(live_audio_lead_seconds) {
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
        // Reverse-debug position coordinate. Unlike `stats`, this is never
        // reset by `reset_stats`; it is only rebased by a snapshot restore.
        self.retired_instructions = self
            .retired_instructions
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

fn realtime_catchup_limit(live_output_lead_seconds: f64) -> Duration {
    let lead = live_output_lead_seconds.max(0.0);
    if lead.is_finite() {
        MAX_REALTIME_CATCHUP + Duration::from_secs_f64(lead)
    } else {
        MAX_REALTIME_CATCHUP
    }
}

#[cfg(test)]
mod tests {
    use super::{
        cck_for_instructions, cpu_cycles_per_instruction_for_clock, instructions_for_cck_value,
        real_cpu_cycles_per_instruction, real_slice_accounting, realtime_budget,
        realtime_catchup_limit, ExecutedSlice, RealPacingBudgetMode, RealPacingProfile,
        DEFAULT_CPU_CYCLES_PER_INSTRUCTION,
    };
    use crate::audio::AudioRuntimeStatus;

    use crate::config::PacingBudget;
    use std::time::{Duration, Instant};

    #[test]
    fn pacing_cost_scales_with_cpu_clock() {
        // Stock 68000 (2 clocks/cck) is the identity. A 7-clocks/cck CPU (an
        // accelerated 030) retires instructions 3.5x faster relative to the
        // chipset, so its per-instruction pacing cost is 3.5x smaller. This is
        // the value a save-state load re-derives when it swaps in a CPU clocked
        // differently from the running config.
        let stock = cpu_cycles_per_instruction_for_clock(2);
        assert_eq!(stock, real_cpu_cycles_per_instruction());
        let accelerated = cpu_cycles_per_instruction_for_clock(7);
        assert!((stock / accelerated - 3.5).abs() < 1e-9);
        // A zero clock is clamped to 1 rather than dividing by zero.
        assert!(cpu_cycles_per_instruction_for_clock(0).is_finite());
    }

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
                prebuffering: false,
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
    fn real_mode_uses_stock_realtime_budget() {
        let cpu_cycles_per_instruction = DEFAULT_CPU_CYCLES_PER_INSTRUCTION;
        let target = super::real_target_instructions_per_second(cpu_cycles_per_instruction);
        assert_eq!(
            realtime_budget(cpu_cycles_per_instruction),
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

    #[test]
    fn large_stall_guard_allows_live_audio_prebuffer_lead() {
        assert_eq!(realtime_catchup_limit(0.0), Duration::from_millis(100));
        assert_eq!(realtime_catchup_limit(0.150), Duration::from_millis(250));
        assert_eq!(realtime_catchup_limit(0.300), Duration::from_millis(400));
    }

    struct ResetTrackingAudio {
        resets: std::rc::Rc<std::cell::RefCell<u32>>,
    }

    impl crate::audio::AudioSink for ResetTrackingAudio {
        fn push(&mut self, _left: f32, _right: f32) {}

        fn flush(&mut self) {}

        fn reset_live_output_after_timeline_jump(&mut self) {
            *self.resets.borrow_mut() += 1;
        }
    }

    fn emulator_with_audio(audio: Box<dyn crate::audio::AudioSink>) -> super::Emulator {
        let mut rom = vec![0u8; crate::memory::ROM_SIZE];
        rom[0..4].copy_from_slice(&0x0000_4000u32.to_be_bytes());
        rom[4..8].copy_from_slice(&0x00F8_0010u32.to_be_bytes());
        rom[0x10..0x12].copy_from_slice(&0x60FEu16.to_be_bytes());

        let bus = crate::bus::Bus::new(
            crate::memory::Memory {
                chip_ram: vec![0u8; 512 * 1024],
                slow_ram: Vec::new(),
                rom,
                overlay: false,
                zorro: crate::zorro::ZorroChain::default(),
                extended_rom: Vec::new(),
                extended_rom_base: 0,
            },
            crate::chipset::paula::Paula::new(Box::new(crate::serial::NullSerialSink), audio),
            crate::floppy::FloppyController::default(),
        );
        super::Emulator::new(
            bus,
            crate::config::CpuModel::M68000,
            false,
            crate::config::PacingBudget::Cycles,
            2,
            false,
        )
        .unwrap()
    }

    #[test]
    fn save_state_load_resets_live_audio_queue_for_new_timeline() {
        let resets = std::rc::Rc::new(std::cell::RefCell::new(0));
        let mut emu = emulator_with_audio(Box::new(ResetTrackingAudio {
            resets: std::rc::Rc::clone(&resets),
        }));
        let path = std::env::temp_dir().join(format!(
            "copperline-emulator-audio-reset-{}.clstate",
            std::process::id()
        ));

        emu.save_state(&path).unwrap();
        emu.load_state(&path).unwrap();

        assert_eq!(*resets.borrow(), 1);
        let _ = std::fs::remove_file(path);
    }
}
