// SPDX-License-Identifier: GPL-3.0-or-later

//! Top-level emulator: owns the M68K CPU instance and drives execution
//! in fixed-size instruction slices, advancing the raster after each
//! slice and raising chipset/CIA/Paula interrupts.

use crate::audio::{audio_profile_enabled, AudioRuntimeStatus, AudioSink};
use crate::bus::Bus;
use crate::chipset::paula::Paula;
use crate::config::{Config, CpuModel, PacingBudget};
use crate::cpu;
use crate::floppy::FloppyController;
use crate::memory::Memory;
use crate::serial::StdoutSink;
use crate::timebase::{Duration, Instant};
use anyhow::{anyhow, bail, Context, Result};
use log::{info, warn};

const INSTRUCTIONS_PER_SLICE: usize = 32_000;
const INSTRUCTIONS_PER_REALTIME_SLICE: usize = 8_192;
/// Longest blind STOP-state fast-forward, in colour clocks, while a serial
/// sink with a live host input side is attached (see the cap site in
/// `idle_fast_forward_chunk`). 256 cck is about 72 microseconds: well under
/// one character time even at 115200 baud (about 10 x 31 cck), so a byte
/// arriving from the host mid-nap raises RBF and wakes the CPU before a
/// second byte can complete and overrun Paula's one-word receive buffer.
const SERIAL_LIVE_IDLE_CAP_CCK: u32 = 256;
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
    /// Run-ahead burst phase (see `set_runahead_phase`): while set, the
    /// per-frame pacing sleep inside `step_real` is suppressed.
    runahead_phase: bool,
    /// Whether the frame currently being stepped is speculative. Host audio
    /// and serial output are withheld, and committed-frame statistics do not
    /// count work that the next burst will re-emulate.
    runahead_speculative: bool,
    cpu_cycles_per_instruction: f64,
    real_pacing_budget_mode: RealPacingBudgetMode,
    /// Fast batch/trace-JIT CPU execution (`[cpu] jit`). The run loop then
    /// hands the machine multi-instruction slices instead of cycle-stepping
    /// one instruction at a time; see `M68kMachine::step_slice_jit`.
    cpu_jit: bool,
    audio_profile: AudioRuntimeProfile,
    real_pacing_profile: RealPacingProfile,
    /// Unfinished host execution quantum left by instruction-granular control
    /// stepping. Keeping this cursor in the emulator makes a debugger pause a
    /// transparent split of the same quantum instead of starting a fresh one
    /// when frame-granular execution resumes.
    realtime_quantum_remaining: usize,
    realtime_quantum_cpu_idle: bool,
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
    /// A running control-protocol profile capture (`profile.start`);
    /// host-side only, never serialized.
    #[cfg(feature = "control")]
    profile: Option<crate::profile::ProfileCapture>,
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

/// True when the opword is a call that, once its callee returns, resumes at the
/// following instruction: BSR (`0x61xx`), JSR (`0x4E80..=0x4EBF`), or TRAP #n
/// (`0x4E40..=0x4E4F`). Step-over runs to the instruction after one of these.
fn instruction_returns_inline(op: u16) -> bool {
    (op & 0xFF00) == 0x6100 || (op & 0xFFC0) == 0x4E80 || (op & 0xFFF0) == 0x4E40
}

/// True when the opword returns from a subroutine or exception: RTE (`0x4E73`),
/// RTD (`0x4E74`), RTS (`0x4E75`), or RTR (`0x4E77`). Step-out watches for one
/// of these lifting the stack pointer past the entry frame.
fn instruction_is_return(op: u16) -> bool {
    matches!(op, 0x4E73 | 0x4E74 | 0x4E75 | 0x4E77)
}

#[derive(Default)]
pub struct EmuStats {
    pub frames: u64,
    pub slices: u64,
    pub instructions: u64,
    pub started_at: Option<crate::timebase::Instant>,
    /// Host time spent emulating in `step_real`, real-time pacing sleeps
    /// excluded. Cleared with the rest of the stats on a guest reset.
    pub busy: Duration,
    /// Times the real-time pacer fell beyond the catch-up limit and dropped
    /// emulated time by re-anchoring instead of chasing the deficit (the
    /// self-heal in `sleep_until_realtime_device_time`).
    pub pacer_slips: u32,
}

/// Snapshot of the always-on performance counters behind the window's
/// performance overlay and the control protocol's `status` report:
/// cumulative host emulation time (pacing sleeps excluded) and pacer slip
/// events, both since the last guest reset.
#[derive(Clone, Copy, Debug, Default)]
pub struct PerfCounters {
    pub busy: Duration,
    pub pacer_slips: u32,
}

impl Emulator {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        bus: Bus,
        cpu_model: CpuModel,
        fpu_enabled: bool,
        cpu_unimplemented: crate::config::UnimplementedPolicy,
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
        let machine = cpu::build(
            bus,
            cpu_model,
            fpu_enabled,
            cpu_clocks_per_cck,
            cpu_unimplemented,
            false,
        )?;
        Ok(Self {
            machine,
            stats: EmuStats::default(),
            paced,
            runahead_phase: false,
            runahead_speculative: false,
            cpu_cycles_per_instruction,
            real_pacing_budget_mode,
            cpu_jit: false,
            audio_profile: AudioRuntimeProfile::new(),
            real_pacing_profile: RealPacingProfile::new(),
            realtime_quantum_remaining: 0,
            realtime_quantum_cpu_idle: false,
            retired_instructions: 0,
            tt_ring: None,
            tt_input: None,
            tt_rwatch: None,
            descriptor: crate::config::MachineDescriptor::default(),
            #[cfg(feature = "control")]
            profile: None,
        })
    }

    /// Install the opt-in 68020/030 CACR-controlled cache models.
    pub fn set_cache_emulation(&mut self, icache: bool, dcache: bool) {
        self.machine.set_cache_emulation(icache, dcache);
    }

    /// Enable the fast batch/trace-JIT CPU mode (`[cpu] jit`). Not
    /// cycle-exact: the CPU runs like an accelerator card. Forces the
    /// cycles pacing budget, whose per-slice debit is derived from the
    /// device time that actually elapsed -- the instruction-count budget
    /// assumes the calibrated interpreter cost per instruction, which the
    /// JIT's flat billing deliberately undercuts.
    pub fn set_cpu_jit(&mut self, on: bool) {
        self.cpu_jit = on;
        self.machine.set_jit_enabled(on);
        if on {
            self.real_pacing_budget_mode = RealPacingBudgetMode::M68kCycles;
            log::info!(
                "cpu jit: batch/trace execution enabled (not cycle-exact, \
                 accelerator-style timing)"
            );
        }
    }

    /// Record the shape of the running machine (from the boot `Config`) and
    /// fingerprint its in-memory ROM. The descriptor is stamped into save
    /// states and compared against a loaded state's stamp.
    pub fn set_machine_descriptor(&mut self, descriptor: crate::config::MachineDescriptor) {
        self.descriptor = descriptor;
        self.refresh_rom_fingerprint();
    }

    /// The shape descriptor of the running machine: stamped from the boot
    /// `Config` by `build_machine` and adopted from the state on a
    /// successful `load_state`, so it always describes the machine as it
    /// stands.
    pub fn machine_descriptor(&self) -> &crate::config::MachineDescriptor {
        &self.descriptor
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

    /// Whether a control-protocol profile capture is running.
    #[cfg(feature = "control")]
    pub fn profile_active(&self) -> bool {
        self.profile.is_some()
    }

    /// The frame analyzer pane closed while a capture runs: the capture
    /// adopts the arming and disarms it at stop.
    #[cfg(feature = "control")]
    pub fn profile_adopt_frame_analyzer(&mut self) {
        let demote_full = self
            .profile
            .as_mut()
            .is_some_and(crate::profile::ProfileCapture::adopt_frame_analyzer);
        if demote_full {
            self.bus_mut().set_frame_analyzer_full(false);
        }
    }

    #[cfg(feature = "control")]
    pub fn profile_status_value(&self) -> serde_json::Value {
        self.profile.as_ref().map_or_else(
            || serde_json::json!({ "active": false }),
            |profile| profile.status_value(true),
        )
    }

    /// Start a profile capture. Refused while one is running: closing
    /// the active capture here would have to invent its summary metadata
    /// (machine descriptor, resources) and could swallow its final write
    /// errors -- the caller stops it first, with a real summary. The
    /// frame analyzer's per-slot trace feeds the records, so it is armed
    /// for the whole session -- which blocks run-ahead, the analyzer's
    /// existing rule -- and disarmed at stop only if the capture armed
    /// (or adopted) it.
    #[cfg(feature = "control")]
    pub fn profile_start(&mut self, opts: crate::profile::ProfileOptions) -> std::io::Result<()> {
        if self.profile.is_some() {
            return Err(std::io::Error::other(
                "a profile capture is already running; stop it first",
            ));
        }
        if opts.memory && opts.trigger.is_some() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "memory snapshots cannot be combined with a deferred trigger",
            ));
        }
        let requested_full = opts.slots;
        let already = self.bus().frame_analyzer_enabled();
        let already_full = self.bus().frame_analyzer_full();
        let frame = self.bus().emulated_frames();
        let seconds = self.bus().emulated_seconds();
        let retired = self.retired_instructions();
        let clocks_per_cck = self.machine.cpu_clocks_per_cck();
        let capture = crate::profile::ProfileCapture::create(
            opts,
            frame,
            seconds,
            retired,
            !already,
            requested_full && !already_full,
            clocks_per_cck,
        )?;
        if capture.options().memory {
            std::fs::write(capture.dir().join("chip-ram.bin"), &self.bus().mem.chip_ram)?;
            std::fs::write(capture.dir().join("slow-ram.bin"), &self.bus().mem.slow_ram)?;
        }
        // Arm and restart only after every fallible output setup step. A
        // refused capture must leave an already-open analyzer's current and
        // completed traces intact.
        self.bus_mut().set_frame_analyzer_enabled(true);
        if requested_full {
            self.bus_mut().set_frame_analyzer_full(true);
        }
        self.bus_mut().restart_frame_analyzer_trace();
        if capture.options().samples {
            self.machine.start_profile_samples(
                capture.options().unwind.clone(),
                capture.options().code_ranges.clone(),
                capture.options().registers,
            );
        }
        log::info!(
            "profile: capturing up to {} frame(s) into {} (slots {}, screenshots {}, pc {}, samples {})",
            capture.options().frames,
            capture.dir().display(),
            capture.options().slots,
            capture.options().screenshots.name(),
            capture.options().pc_samples,
            capture.options().samples,
        );
        self.profile = Some(capture);
        Ok(())
    }

    /// Stop a running capture: optional final screenshot, the
    /// `profile.json` summary, analyzer disarm if this capture armed (or
    /// adopted) it. Returns the final status, or `{"active": false}`.
    #[cfg(feature = "control")]
    pub fn profile_stop(
        &mut self,
        machine: serde_json::Value,
        resources: serde_json::Value,
    ) -> std::io::Result<serde_json::Value> {
        let Some(mut capture) = self.profile.take() else {
            return Ok(serde_json::json!({ "active": false }));
        };
        capture.set_stack_bounds(crate::amigaos::stack_bounds_on_bus(self.bus()));
        self.machine.stop_profile_samples();
        if matches!(
            capture.options().screenshots,
            crate::profile::ScreenshotMode::Last
        ) {
            let (fb, lines, width) = crate::control::exec::render_frame(self);
            let path = capture.dir().join("frame-last.png");
            if let Err(e) =
                crate::screenshot::save(&path, &fb[..width * lines], width as u32, lines as u32)
            {
                log::warn!("profile: final screenshot failed: {e:#}");
            }
        }
        if capture.armed_analyzer() {
            self.bus_mut().set_frame_analyzer_enabled(false);
        } else if capture.armed_full() {
            self.bus_mut().set_frame_analyzer_full(false);
        }
        let frame = self.bus().emulated_frames();
        let seconds = self.bus().emulated_seconds();
        let rom_symbols = crate::amigaos::symbols::snapshot_on_bus(self.bus());
        let status = capture.finish(machine, resources, rom_symbols, frame, seconds)?;
        log::info!(
            "profile: {} frame(s) written to {}",
            status["frames_written"],
            capture.dir().display()
        );
        Ok(status)
    }

    /// One committed frame for a running capture: gather, then append.
    #[cfg(feature = "control")]
    fn profile_poll(&mut self) -> Result<()> {
        use serde_json::{json, Value};
        let Some(opts) = self
            .profile
            .as_ref()
            .filter(|profile| !profile.done())
            .map(|profile| profile.options().clone())
        else {
            return Ok(());
        };
        let frame = self.bus().emulated_frames();
        match self.profile.as_ref().and_then(|p| p.last_frame_seen()) {
            Some(last) if frame == last => return Ok(()),
            Some(last) if frame < last => {
                let retired = self.retired_instructions();
                self.machine.discard_profile_samples();
                if let Some(profile) = self.profile.as_mut() {
                    profile
                        .note_reposition(frame, retired)
                        .map_err(|e| anyhow!("profile reposition marker: {e}"))?;
                }
                return Ok(());
            }
            _ => {}
        }

        let retired_now = self.retired_instructions();
        let busy_cck = {
            let bus = self.bus();
            if let Some((idle, total)) = bus
                .uaelib
                .as_ref()
                .and_then(|uaelib| uaelib.idle().last_frame())
            {
                total.saturating_sub(idle)
            } else {
                bus.frame_bus_trace()
                    .map(|trace| trace.rows as u64 * u64::from(trace.line_cck))
                    .unwrap_or(0)
            }
        };
        if self
            .profile
            .as_mut()
            .is_some_and(|profile| !profile.should_record(frame, busy_cck, retired_now))
        {
            self.machine.discard_profile_samples();
            return Ok(());
        }

        let instruction_samples = if opts.samples {
            self.machine.take_profile_samples()
        } else {
            Vec::new()
        };

        // Renders first: they borrow the whole emulator immutably.
        let (mut screenshot, mut digest) = (None, None);
        if matches!(opts.screenshots, crate::profile::ScreenshotMode::Every) {
            let (fb, lines, width) = crate::control::exec::render_frame(self);
            let name = format!("frame-{frame:06}.png");
            match crate::screenshot::save(
                &opts.path.join(&name),
                &fb[..width * lines],
                width as u32,
                lines as u32,
            ) {
                Ok(()) => {
                    digest = Some(format!(
                        "{:016x}",
                        crate::control::exec::fnv1a64(&fb[..width * lines])
                    ));
                    screenshot = Some(name);
                }
                Err(e) => log::warn!("profile: screenshot for frame {frame} failed: {e:#}"),
            }
        }

        let bus = self.bus();
        let mut full_slot_records = None;
        let seconds = bus.emulated_seconds();
        let idle_cck = bus
            .uaelib
            .as_ref()
            .and_then(|uaelib| uaelib.idle().last_frame())
            .map(|(idle, _)| idle);
        let pc = opts.pc_samples.then(|| self.machine.pc());
        let mut record = json!({
            "frame": frame,
            "seconds": seconds,
            "idle_cck": idle_cck,
            "traced": false,
        });
        if let Some(trace) = bus.frame_bus_trace() {
            let owners: Value = crate::bus::CHIP_BUS_OWNER_NAMES
                .iter()
                .zip(trace.owner_cck.iter())
                .map(|(name, cck)| (name.to_string(), Value::from(*cck)))
                .collect::<serde_json::Map<String, Value>>()
                .into();
            let starve: Value = crate::bus::CHIP_BUS_OWNER_NAMES
                .iter()
                .zip(trace.blitter_starve_cck.iter())
                .filter(|(_, cck)| **cck != 0)
                .map(|(name, cck)| (name.to_string(), Value::from(*cck)))
                .collect::<serde_json::Map<String, Value>>()
                .into();
            let blits: Vec<Value> = trace
                .blits
                .iter()
                .map(|blit| {
                    json!({
                        "id": blit.id,
                        "bltcon0": format!("{:#06X}", blit.bltcon0),
                        "bltcon1": format!("{:#06X}", blit.bltcon1),
                        "width_words": blit.width_words,
                        "height": blit.height,
                        "descending": blit.descending,
                        "line_mode": blit.line_mode,
                        "fill_mode": blit.fill_mode,
                        "channels": blit.channels,
                        "apt": format!("{:#010X}", blit.apt),
                        "bpt": format!("{:#010X}", blit.bpt),
                        "cpt": format!("{:#010X}", blit.cpt),
                        "dpt": format!("{:#010X}", blit.dpt),
                        "modulos": blit.modulos,
                        "shifts": blit.shifts,
                        "masks": blit.masks.map(|value| format!("0x{value:04X}")),
                        "minterm": format!("0x{:02X}", blit.minterm),
                        "formula": crate::blitviz::minterm_formula(blit.minterm),
                        "data": blit.data.map(|value| format!("0x{value:04X}")),
                        "start_frame": blit.start_frame,
                        "end_frame": blit.end_frame,
                        "start": [blit.start.0, blit.start.1],
                        "end": blit.end.map(|(v, h)| json!([v, h])),
                        "cycles_used": blit.cycles_used,
                        "cycles_stalled": blit.cycles_stalled,
                        "captured_words": blit.channel_words.each_ref().map(Vec::len),
                        "render_truncated": blit.render_truncated,
                    })
                })
                .collect();
            record["traced"] = Value::Bool(true);
            record["rows"] = Value::from(trace.rows);
            record["line_cck"] = Value::from(trace.line_cck);
            record["cck_length"] = Value::from(trace.rows as u64 * u64::from(trace.line_cck));
            record["owner_cck"] = owners;
            record["blitter"] = json!({
                "busy_cck": trace.blitter_busy_cck,
                "starve_cck": starve,
            });
            record["blits"] = Value::from(blits);
            record["partial"] = Value::Bool(trace.partial);
            record["registers"] = json!({
                "custom": trace.registers.custom,
                "chipset_flags": trace.registers.chipset_flags,
                "palette": {
                    "hi": trace.registers.palette_hi,
                    "lo": trace.registers.palette_lo,
                },
            });
            let wait_by: Value = crate::bus::CPU_WAIT_CLASS_NAMES
                .iter()
                .zip(trace.cpu_wait_by_class.iter())
                .filter(|(_, cck)| **cck != 0)
                .map(|(name, cck)| (name.to_string(), Value::from(*cck)))
                .collect::<serde_json::Map<String, Value>>()
                .into();
            let wait_by_kind: Value = crate::bus::CPU_BUS_ACCESS_KIND_NAMES
                .iter()
                .zip(trace.cpu_wait_by_kind.iter())
                .filter(|(_, cck)| **cck != 0)
                .map(|(name, cck)| (name.to_string(), Value::from(*cck)))
                .collect::<serde_json::Map<String, Value>>()
                .into();
            let stall_pcs: Vec<Value> = trace
                .top_stalled_pcs(crate::profile::PROFILE_STALL_PCS)
                .into_iter()
                .map(|(pc, cck)| json!({"pc": format!("{pc:#010X}"), "cck": cck}))
                .collect();
            record["cpu"] = json!({
                "wait_cck": trace.cpu_wait_cck,
                "wait_by": wait_by,
                "wait_by_kind": wait_by_kind,
                "stall_pcs": stall_pcs,
                "stall_pcs_distinct": trace.stalled_pc_count(),
                "stall_pcs_other": trace.cpu_wait_pc_other,
            });
            if opts.slots {
                let rows: Vec<Value> = (0..trace.rows)
                    .filter_map(|vpos| trace.owner_row(vpos))
                    .map(|row| Value::from(crate::profile::rle_owner_row(row)))
                    .collect();
                record["slots"] = Value::from(rows);
                let waits: Vec<Value> = (0..trace.rows)
                    .filter_map(|vpos| trace.cpu_wait_row(vpos))
                    .map(|row| Value::from(crate::profile::rle_owner_row(row)))
                    .collect();
                record["cpu_wait"] = Value::from(waits);
                full_slot_records = trace.records_arc();
                let instantaneous: Vec<Value> = trace
                    .instantaneous_records()
                    .iter()
                    .map(|entry| {
                        let slot = &entry.record;
                        json!({
                            "vpos": entry.vpos,
                            "hpos": entry.hpos,
                            "reg": slot.reg,
                            "addr": slot.addr,
                            "data": format!("0x{:016X}", slot.data),
                            "size": slot.size,
                            "kind": slot.kind,
                            "subtype": slot.subtype,
                            "ipl": slot.ipl,
                            "flags": slot.flags,
                            "events": slot.events,
                            "event_names": crate::bus::bus_event_names(slot.events),
                        })
                    })
                    .collect();
                if !instantaneous.is_empty() {
                    record["instantaneous_slots"] = Value::from(instantaneous);
                }
            }
        }
        if let Some(pc) = pc {
            record["pc"] = Value::from(format!("{pc:#010X}"));
        }
        if let Some(name) = screenshot {
            record["screenshot"] = Value::from(name);
            record["digest"] = Value::from(digest.unwrap_or_default());
        }

        if let Some(profile) = self.profile.as_mut() {
            record["retired"] = Value::from(profile.take_retired_delta(retired_now));
            if let Some(records) = full_slot_records.as_deref() {
                let name = profile
                    .write_slots(frame, records)
                    .map_err(|e| anyhow!("profile slots: {e}"))?;
                record["slots_file"] = Value::from(name);
                record["slots_record_bytes"] = Value::from(crate::bus::BusSlotRecord::BYTE_SIZE);
            }
            if opts.samples {
                let stats = profile
                    .write_samples(frame, &instruction_samples)
                    .map_err(|e| anyhow!("profile samples: {e}"))?;
                record["samples"] = Value::from(stats.samples_name);
                record["samples_meta"] = Value::from(stats.metadata_name);
                record["sample_count"] = Value::from(stats.count);
                record["samples_total"] = Value::from(stats.samples_total);
                record["irq_cck"] = Value::from(stats.irq_cck);
            }
            profile
                .record(frame, &record)
                .map_err(|e| anyhow!("profile record: {e}"))?;
        }
        if self.profile.as_ref().is_some_and(|profile| profile.done()) {
            self.machine.stop_profile_samples();
        }
        Ok(())
    }

    /// Whether the WinUAE-compatible uaelib trap is fitted
    /// (`[emulation] uaelib`, `crate::uaelib`).
    pub fn uaelib_fitted(&self) -> bool {
        self.bus().uaelib.is_some()
    }

    /// The fitted freezer cartridge (`[cartridge] model`,
    /// `crate::cartridge`), if any.
    pub fn cartridge(&self) -> Option<&crate::cartridge::Cartridge> {
        self.bus().cartridge.as_ref()
    }

    pub fn cartridge_fitted(&self) -> bool {
        self.bus().cartridge.is_some()
    }

    /// Press the freezer cartridge's button. The freeze is journaled for
    /// reverse-debug replay with the VBR it was delivered under. Returns
    /// the monitor's entry address the level-7 vector now holds.
    pub fn cartridge_freeze(&mut self) -> Result<u32> {
        let Some(cartridge) = self.cartridge() else {
            return Err(crate::cartridge::FreezeError::NotFitted.into());
        };
        let model = cartridge.model();
        let vbr = self.machine.vbr();
        let entry = self.machine.cartridge_freeze()?;
        log::info!(
            "cartridge: {} freeze, level-7 vector at {:#010X} -> {entry:#010X}",
            model.label(),
            vbr.wrapping_add(0x7C)
        );
        self.tt_note_input(crate::inputsched::ReplayAction::Freeze { vbr });
        Ok(entry)
    }

    /// The guest's latest `warpmode()` request through the uaelib trap,
    /// if any since the last take.
    pub fn take_uaelib_warp_request(&mut self) -> Option<bool> {
        self.bus_mut()
            .uaelib
            .as_mut()
            .and_then(|u| u.take_warp_request())
    }

    /// Queued guest debug events (uaelib functions 86 and 88) and the
    /// number dropped since the last take.
    pub fn take_uaelib_debug_events(&mut self) -> (Vec<crate::uaelib::DebugEvent>, u64) {
        self.bus_mut()
            .uaelib
            .as_mut()
            .map(|u| u.take_debug_events())
            .unwrap_or_default()
    }

    /// Queued guest debug lines for the debugger console pane (uaelib
    /// function 86), mirroring the stdout echo.
    pub fn take_uaelib_console_lines(&mut self) -> Vec<String> {
        self.bus_mut()
            .uaelib
            .as_mut()
            .map_or_else(Vec::new, |uaelib| uaelib.take_console_lines())
    }

    pub fn clear_uaelib_debug_events(&mut self) {
        if let Some(uaelib) = self.bus_mut().uaelib.as_mut() {
            uaelib.clear_debug_events();
        }
    }

    /// The overlay display list the guest drew through the uaelib trap.
    pub fn uaelib_overlay(&self) -> &[crate::uaelib::OverlayCmd] {
        self.bus().uaelib.as_ref().map_or(&[], |u| u.overlay())
    }

    /// The resources the guest registered through the uaelib trap.
    pub fn uaelib_resources(&self) -> &[crate::uaelib::DebugResource] {
        self.bus().uaelib.as_ref().map_or(&[], |u| u.resources())
    }

    /// Export a registered bitmap or palette to PNG using the same decoder
    /// as the frame analyzer's Resources tab.
    pub fn export_uaelib_resource(
        &self,
        address: u32,
        path: &std::path::Path,
    ) -> Result<ResourceExport> {
        use crate::uaelib::{
            ResourceKind, RESOURCE_FLAG_HAM, RESOURCE_FLAG_INTERLEAVED, RESOURCE_FLAG_MASKED,
        };
        use crate::video::resource_preview;

        let resource = self
            .uaelib_resources()
            .iter()
            .find(|resource| resource.address == address)
            .ok_or_else(|| anyhow!("no registered debug resource at ${address:06X}"))?;
        let (pixels, width, height, note) = match resource.kind {
            ResourceKind::Bitmap {
                width,
                height,
                planes,
            } => {
                let data = self
                    .machine
                    .debug_read_memory(address, (resource.size.max(1) as usize).min(512 * 1024));
                let palette = self.uaelib_resource_palette();
                let preview = resource_preview::decode_bitmap(
                    &data,
                    width,
                    height,
                    planes,
                    resource.flags & RESOURCE_FLAG_INTERLEAVED != 0,
                    resource.flags & RESOURCE_FLAG_MASKED != 0,
                    resource.flags & RESOURCE_FLAG_HAM != 0,
                    &palette,
                );
                if preview.width == 0 || preview.height == 0 {
                    bail!("resource {} has degenerate bitmap geometry", resource.name);
                }
                (preview.pixels, preview.width, preview.height, preview.note)
            }
            ResourceKind::Palette { entries } => {
                let data = self
                    .machine
                    .debug_read_memory(address, usize::from(entries) * 2);
                let colours = resource_preview::decode_palette_words(&data, entries);
                if colours.is_empty() {
                    bail!("resource {} has no palette entries", resource.name);
                }
                let columns = colours.len().min(16);
                let rows = colours.len().div_ceil(columns);
                let cell = 16usize;
                let width = columns * cell;
                let height = rows * cell;
                let mut pixels = vec![0xFF20_2020; width * height];
                for (index, colour) in colours.into_iter().enumerate() {
                    let x0 = (index % columns) * cell;
                    let y0 = (index / columns) * cell;
                    for y in y0..y0 + cell {
                        pixels[y * width + x0..y * width + x0 + cell].fill(colour);
                    }
                }
                (pixels, width, height, None)
            }
            ResourceKind::Copperlist => bail!("Copperlist resources cannot be exported as PNG"),
            ResourceKind::Unknown(kind) => {
                bail!("resource {} has unknown type {kind}", resource.name)
            }
        };
        crate::screenshot::save(path, &pixels, width as u32, height as u32)?;
        Ok(ResourceExport {
            name: resource.name.clone(),
            kind: resource.kind_name(),
            width,
            height,
            note,
        })
    }

    fn uaelib_resource_palette(&self) -> Vec<u32> {
        if let Some((address, entries)) =
            self.uaelib_resources()
                .iter()
                .find_map(|resource| match resource.kind {
                    crate::uaelib::ResourceKind::Palette { entries } => {
                        Some((resource.address, entries))
                    }
                    _ => None,
                })
        {
            let data = self
                .machine
                .debug_read_memory(address, usize::from(entries) * 2);
            return crate::video::resource_preview::decode_palette_words(&data, entries);
        }
        let palette = &self.bus().denise.palette;
        (0..32)
            .map(|entry| crate::chipset::denise::rgb24_to_rgba8(palette.rgb24(entry)))
            .collect()
    }

    /// The guest's idle-marker accounting, when the trap is fitted.
    pub fn uaelib_idle(&self) -> Option<&crate::uaelib::IdleAccounting> {
        self.bus().uaelib.as_ref().map(|u| u.idle())
    }

    /// Suspend only host live audio output. Emulated Paula time still
    /// advances whenever the machine is stepped.
    pub fn set_live_audio_suspended(&mut self, suspended: bool) {
        self.bus_mut().set_live_audio_suspended(suspended);
    }

    /// Discard host audio queued for an emulated timeline that has just been
    /// abandoned. Paula's serialized DMA and mixer state is left untouched;
    /// the serial sink is handled once, while host resources are adopted by
    /// the restored bus.
    pub fn reset_live_audio_after_timeline_jump(&mut self) {
        self.bus_mut().reset_live_audio_after_timeline_jump();
    }

    pub fn keyboard_reset(&mut self) -> Result<()> {
        log::info!("keyboard reset pulse");
        self.bus_mut().reset_for_keyboard_reset();
        self.machine.reset_after_bus_reset();
        self.stats = EmuStats::default();
        self.reset_realtime_quantum();
        Ok(())
    }

    /// Cold power-on reset: reinitialises RAM with the configured policy and
    /// returns the machine to its fresh power-cycled state, distinct from the
    /// warm keyboard reset.
    pub fn power_on_reset(&mut self) -> Result<()> {
        log::info!("cold power-on reset");
        self.bus_mut().power_on_reset();
        self.machine.reset_after_bus_reset();
        self.stats = EmuStats::default();
        self.reset_realtime_quantum();
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
    ///
    /// Quiesces the copperhf.device board first (a no-op unless one is
    /// configured and something is in flight): the worker-thread I/O
    /// pipeline must be fully drained before its board is serialized (see
    /// `src/copperhf.rs`'s module doc).
    pub fn save_state(&mut self, path: &std::path::Path) -> Result<()> {
        self.bus_mut().copperhf_quiesce();
        crate::savestate::save(&self.machine, &self.descriptor, path)
    }

    /// `save_state` into memory instead of a file, for hosts with no
    /// filesystem to write to (the browser build hands the blob to a
    /// download or IndexedDB). Same bytes, same format version.
    pub fn save_state_bytes(&mut self) -> Result<Vec<u8>> {
        self.bus_mut().copperhf_quiesce();
        let mut blob = Vec::new();
        crate::savestate::save_to_writer(&self.machine, &self.descriptor, &mut blob)?;
        Ok(blob)
    }

    /// Restore a save state from `path`. The state carries its own machine
    /// (RAM, ROM, chip revisions, CPU), so a load fully rebuilds it; when that
    /// machine differs from the one running, the host is reconfigured to match
    /// the state (the descriptor is adopted and pacing re-derived) and the
    /// difference is logged. On success emulated time jumps to the state's
    /// timeline, so the real-time pacing anchor is re-baselined to "now"; on
    /// failure the running machine is untouched.
    pub fn load_state(&mut self, path: &std::path::Path) -> Result<StateLoadOutcome> {
        self.adopt_loaded_state(|machine| crate::savestate::load(machine, path))
    }

    /// `load_state` from bytes instead of a file: the browser counterpart,
    /// restoring a state that came from a picked file or IndexedDB. The
    /// blob is a whole state file, so a desktop `.clstate` loads here and
    /// vice versa; a failed parse leaves the running machine untouched.
    pub fn load_state_bytes(&mut self, blob: &[u8]) -> Result<StateLoadOutcome> {
        self.adopt_loaded_state(|machine| crate::savestate::load_from_reader(machine, blob))
    }

    /// Shared tail of both load paths: restore through `load`, then put the
    /// host back in step with the machine that came out of it.
    fn adopt_loaded_state(
        &mut self,
        load: impl FnOnce(&mut cpu::M68kMachine) -> Result<crate::config::MachineDescriptor>,
    ) -> Result<StateLoadOutcome> {
        // Channel mode, stereo separation, and the filter override are host
        // preferences, not part of the saved machine, so carry the current
        // choices across the load.
        let uaelib_file_authority = self
            .bus()
            .uaelib
            .as_ref()
            .and_then(crate::uaelib::UaeLib::file_authority);
        let mono = self.bus_mut().paula.mono_output();
        let separation = self.bus_mut().paula.stereo_separation();
        let filter = self.bus_mut().paula.led_filter_mode();
        let loaded = load(&mut self.machine)?;
        if let Some(uaelib) = self.bus_mut().uaelib.as_mut() {
            uaelib.set_file_authority(uaelib_file_authority);
        }
        self.bus_mut().paula.set_mono_output(mono);
        self.bus_mut().paula.set_stereo_separation(separation);
        self.bus_mut().paula.set_led_filter_mode(filter);
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
        self.reset_realtime_quantum();
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
    /// the main thread), and after loading a state whose timeline is already
    /// at a non-zero emulated time. Without re-anchoring, the pacer would see
    /// emulated time far behind or ahead of the wall-clock target and either
    /// sprint or sleep to catch up, corrupting audio/video pacing. The anchor
    /// is placed so that the current emulated device target maps exactly to
    /// now. For paced emulation this also initializes the anchor before the
    /// first step_frame; unpaced headless runs still leave statistics anchored
    /// at their first executed frame.
    pub fn reanchor_realtime_clock(&mut self) {
        if self.stats.started_at.is_none() && !self.paced {
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
        // Republish the serial time base: `started_at` is the host instant of
        // emulated time 0 on the same (audio-lead-adjusted) timeline audio and
        // video are paced against, so a MIDI sink schedules in sync with them.
        if let Some(host_epoch) = self.stats.started_at {
            self.bus_mut()
                .set_serial_time_anchor(crate::serial::SerialTimeAnchor {
                    host_epoch,
                    cck_per_second: f64::from(crate::chipset::paula::PAULA_CLOCK_HZ),
                });
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

    /// Stop recording reverse history and release the retained snapshots.
    /// Reverse ops report "not armed" afterwards until something re-arms.
    pub fn disable_time_travel(&mut self) {
        self.tt_ring = None;
        self.tt_input = None;
    }

    pub fn time_travel_enabled(&self) -> bool {
        self.tt_ring.is_some()
    }

    pub fn time_travel_ring(&self) -> Option<&crate::timetravel::SnapshotRing> {
        self.tt_ring.as_ref()
    }

    /// Capture an initial reverse-debug anchor if reverse mode is armed but no
    /// snapshot has been retained yet. Remote debuggers call this when a GDB
    /// session starts so early reverse-step operations can replay from reset.
    pub fn debug_ensure_time_travel_anchor(&mut self) -> Result<()> {
        if self.tt_ring.as_ref().is_some_and(|ring| !ring.is_empty()) {
            return Ok(());
        }
        self.tt_capture_if_due()
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
        // Preserve host-side channel mode, separation, and filter override
        // across the restore (see load_state).
        let uaelib_file_authority = self
            .bus()
            .uaelib
            .as_ref()
            .and_then(crate::uaelib::UaeLib::file_authority);
        let mono = self.bus_mut().paula.mono_output();
        let separation = self.bus_mut().paula.stereo_separation();
        let filter = self.bus_mut().paula.led_filter_mode();
        let mut cursor = std::io::Cursor::new(blob);
        self.machine.apply_state(&mut cursor)?;
        if let Some(uaelib) = self.bus_mut().uaelib.as_mut() {
            uaelib.set_file_authority(uaelib_file_authority);
        }
        self.bus_mut().paula.set_mono_output(mono);
        self.bus_mut().paula.set_stereo_separation(separation);
        self.bus_mut().paula.set_led_filter_mode(filter);
        self.retired_instructions = pos;
        self.reset_realtime_quantum();
        self.reset_live_audio_after_timeline_jump();
        self.reanchor_realtime_clock();
        Ok(())
    }

    /// Enter or leave the run-ahead burst phase. While in the phase, the
    /// per-frame pacing sleep inside `step_real` is suppressed; the caller
    /// paces once per burst against the anchor frame's end time instead
    /// (`pace_runahead_burst`).
    pub fn set_runahead_phase(&mut self, on: bool) {
        self.runahead_phase = on;
    }

    pub fn runahead_phase(&self) -> bool {
        self.runahead_phase
    }

    /// Mark subsequent emulation as speculative. The machine still executes
    /// normally, but output that cannot be rewound is withheld until the same
    /// guest time is executed as the next committed anchor.
    pub fn set_runahead_speculative(&mut self, on: bool) {
        self.runahead_speculative = on;
        let bus = self.bus_mut();
        bus.set_live_audio_discard(on);
        bus.paula.set_speculative_host_quiet(on);
        if let Some(uaelib) = bus.uaelib.as_mut() {
            uaelib.set_speculative_host_quiet(on);
        }
    }

    /// Serialize the machine at a run-ahead anchor boundary. Same-process
    /// bincode like the reverse-debug ring (no framing), taken at a frame
    /// boundary where `M68kMachine::write_state` is consistent.
    pub fn runahead_snapshot(&self) -> Result<Vec<u8>> {
        self.snapshot_blob()
    }

    /// Capture a same-process rollback checkpoint, including CPU pipeline state.
    /// Valid only for this build; never accept checkpoints from a remote peer.
    pub fn netplay_snapshot(&self) -> Result<Vec<u8>> {
        let mut bytes = Vec::new();
        self.machine.write_rollback_state(&mut bytes)?;
        Ok(bytes)
    }

    /// Restore a local checkpoint from [`Self::netplay_snapshot`], preserving
    /// live host resources and resetting the real-time stepping quantum.
    pub fn netplay_restore(&mut self, blob: &[u8]) -> Result<()> {
        self.machine
            .apply_rollback_state(&mut std::io::Cursor::new(blob))?;
        self.reset_realtime_quantum();
        Ok(())
    }

    /// Restore an anchor snapshot produced by [`Self::runahead_snapshot`].
    /// Unlike [`Self::restore_blob`] this deliberately does NOT reset the
    /// live audio stream or re-anchor real-time pacing: the audible
    /// timeline continues uninterrupted across the rewind, and the pacing
    /// coordinate keeps marching forward while emulated time oscillates
    /// within the burst. Host-side Paula settings survive as usual.
    pub fn runahead_restore(&mut self, blob: &[u8]) -> Result<()> {
        let uaelib_file_authority = self
            .bus()
            .uaelib
            .as_ref()
            .and_then(crate::uaelib::UaeLib::file_authority);
        let mono = self.bus_mut().paula.mono_output();
        let separation = self.bus_mut().paula.stereo_separation();
        let filter = self.bus_mut().paula.led_filter_mode();
        let mut cursor = std::io::Cursor::new(blob);
        self.machine.apply_state(&mut cursor)?;
        if let Some(uaelib) = self.bus_mut().uaelib.as_mut() {
            uaelib.set_file_authority(uaelib_file_authority);
        }
        self.bus_mut().paula.set_mono_output(mono);
        self.bus_mut().paula.set_stereo_separation(separation);
        self.bus_mut().paula.set_led_filter_mode(filter);
        self.reset_realtime_quantum();
        Ok(())
    }

    /// Pace a completed run-ahead burst. `anchor_end_seconds` is the
    /// emulated time at the end of the iteration's anchor (first) frame:
    /// presented frames advance one per iteration, so wall-clock budget per
    /// iteration is one frame period even though the burst retired more.
    /// No-op when unpaced (warp/headless).
    pub fn pace_runahead_burst(&mut self, anchor_end_seconds: f64) {
        if !self.paced {
            return;
        }
        self.pace_to_emulated_target(anchor_end_seconds);
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
        self.tt_capture_now()
    }

    /// Snapshot the machine into the reverse-debug ring at the current
    /// position, regardless of the capture interval. A debugger takes one
    /// at a stop the user will step back from, so the replay starts
    /// there rather than at an older frame boundary: host-side state a
    /// snapshot cannot roll back (a directory mount's file handles, a
    /// hard-disk image) may have changed since, and a replay from before
    /// that traffic diverges. No-op unless reverse mode is armed.
    pub fn debug_time_travel_anchor_now(&mut self) -> Result<()> {
        if self.tt_ring.is_none() {
            return Ok(());
        }
        self.tt_capture_now()
    }

    fn tt_capture_now(&mut self) -> Result<()> {
        let frame = self.bus().emulated_frames();
        let pos = self.retired_instructions;
        let cck = self.bus().emulated_cck();
        let blob = self.snapshot_blob()?;
        if let Some(ring) = self.tt_ring.as_mut() {
            ring.push(crate::timetravel::Snapshot {
                pos,
                frame,
                cck,
                blob,
            });
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
        self.tt_discard_after(target_pos);
        Ok(ReverseOutcome::Found(()))
    }

    /// Forget the reverse history recorded after `pos`. Repositioning the
    /// machine there abandons the timeline that followed: those snapshots
    /// describe a future that will not happen again once new input arrives,
    /// and leaving them in the ring would let one be picked as an anchor as
    /// soon as the position counter climbs past it a second time.
    fn tt_discard_after(&mut self, pos: u64) {
        if let Some(ring) = self.tt_ring.as_mut() {
            ring.truncate_after(pos);
        }
        if let Some(log) = self.tt_input.as_mut() {
            log.prune_after(pos);
        }
    }

    /// Rewind to the newest capture point strictly earlier than the current
    /// position -- the user-facing "rewind one step", as distinct from the
    /// debugger's instruction- and frame-exact reverse steps. It lands on a
    /// snapshot rather than an arbitrary boundary, so it costs one restore and
    /// no replay, and one step covers `rewind_interval_frames` of emulated
    /// time. `BeyondHistory` means nothing earlier is retained.
    pub fn tt_rewind_step(&mut self) -> Result<crate::timetravel::ReverseOutcome<u64>> {
        use crate::timetravel::ReverseOutcome;
        let target = match self
            .tt_ring
            .as_ref()
            .and_then(|r| r.nearest_before(self.retired_instructions))
        {
            Some(s) => s.pos,
            None => return Ok(ReverseOutcome::BeyondHistory),
        };
        Ok(match self.tt_restore_to(target)? {
            ReverseOutcome::Found(()) => ReverseOutcome::Found(self.retired_instructions),
            ReverseOutcome::NotFound => ReverseOutcome::NotFound,
            ReverseOutcome::BeyondHistory => ReverseOutcome::BeyondHistory,
        })
    }

    /// Emulated seconds of rewind history behind the current position. `None`
    /// unless the ring is armed and holds at least one snapshot.
    pub fn rewind_history_seconds(&self) -> Option<f64> {
        let oldest = self.tt_ring.as_ref()?.oldest_cck()?;
        let span = self.bus().emulated_cck().saturating_sub(oldest);
        Some(span as f64 / f64::from(crate::chipset::paula::PAULA_CLOCK_HZ))
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
    pub fn tt_reverse_continue(
        &mut self,
    ) -> Result<crate::timetravel::ReverseOutcome<(u64, String)>> {
        // The full scan honours every armed stop kind: PC breakpoints
        // (addresses; conditions are not replayed), watchpoints, register
        // watches, beam traps, Copper breakpoints, exception catches, and
        // the task catch.
        let breakpoints: Vec<u32> = self
            .machine
            .ui_breaks()
            .breakpoints
            .iter()
            .map(|bp| bp.addr)
            .collect();
        let anything_armed = self.machine.ui_breaks().armed()
            || !self.bus().ui_beam_traps().is_empty()
            || !self.bus().ui_copper_breaks().is_empty();
        if !anything_armed && breakpoints.is_empty() {
            return Ok(crate::timetravel::ReverseOutcome::NotFound);
        }
        self.tt_reverse_continue_impl(&breakpoints, true)
    }

    /// Run backward to the previous PC breakpoint in `breakpoints`. This is
    /// the same operation as `tt_reverse_continue`, but takes an explicit
    /// breakpoint list (and scans nothing else) so remote debugger frontends
    /// can keep their protocol breakpoints independent from the in-window
    /// debugger state.
    pub fn tt_reverse_continue_to(
        &mut self,
        breakpoints: &[u32],
    ) -> Result<crate::timetravel::ReverseOutcome<u64>> {
        use crate::timetravel::ReverseOutcome;
        if breakpoints.is_empty() {
            return Ok(ReverseOutcome::NotFound);
        }
        Ok(match self.tt_reverse_continue_impl(breakpoints, false)? {
            ReverseOutcome::Found((pos, _)) => ReverseOutcome::Found(pos),
            ReverseOutcome::NotFound => ReverseOutcome::NotFound,
            ReverseOutcome::BeyondHistory => ReverseOutcome::BeyondHistory,
        })
    }

    fn tt_reverse_continue_impl(
        &mut self,
        breakpoints: &[u32],
        full: bool,
    ) -> Result<crate::timetravel::ReverseOutcome<(u64, String)>> {
        use crate::timetravel::ReverseOutcome;
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
            if let Some(found) = self.tt_scan_stop(breakpoints, interval_end, full)? {
                let (pos, reason) = found;
                self.tt_restore_to(pos)?;
                // The final forward replay to `pos` re-fires checks along
                // the way; the landing position's story is `reason`, so
                // drop the stray pending stop.
                let _ = self.machine.take_ui_debug_stop();
                return Ok(ReverseOutcome::Found((pos, reason)));
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
    /// boundary (strictly before `end_pos`) where a stop condition held: a PC
    /// breakpoint ("about to execute", like the forward run), or -- when
    /// `full` -- any interactive stop the forward machinery reports
    /// (watchpoints, register watches, beam traps, Copper breakpoints,
    /// catches). The restore that began this interval re-baselined the
    /// watch compare values, so hits mean "changed during replay".
    fn tt_scan_stop(
        &mut self,
        breakpoints: &[u32],
        end_pos: u64,
        full: bool,
    ) -> Result<Option<(u64, String)>> {
        // A0-A23 on the 24-bit models, the full 32 bits on 020+, so a
        // breakpoint in RAM above the 24-bit space is not aliased onto a
        // different PC during replay.
        let pc_mask = self.machine.ui_addr_mask();
        let is_bp = |pc: u32| breakpoints.contains(&(pc & pc_mask));
        // Drain any stop left over from an earlier replay segment.
        let _ = self.machine.take_ui_debug_stop();
        self.tt_apply_due_input(self.retired_instructions);
        let mut best: Option<(u64, String)> = None;
        if self.retired_instructions < end_pos && is_bp(self.machine.pc()) {
            best = Some((
                self.retired_instructions,
                format!("Breakpoint at ${:06X}", self.machine.pc() & pc_mask),
            ));
        }
        let mut cpu_idle = false;
        let mut guard: u64 = 0;
        while self.retired_instructions < end_pos {
            let before = self.retired_instructions;
            self.run_one_step(&mut cpu_idle, INSTRUCTIONS_PER_SLICE)?;
            self.tt_apply_due_input(self.retired_instructions);
            if full {
                if let Some(stop) = self.machine.take_ui_debug_stop() {
                    if self.retired_instructions < end_pos {
                        best = Some((self.retired_instructions, stop.describe()));
                    }
                }
            }
            if self.retired_instructions < end_pos && is_bp(self.machine.pc()) {
                best = Some((
                    self.retired_instructions,
                    format!("Breakpoint at ${:06X}", self.machine.pc() & pc_mask),
                ));
            }
            if self.retired_instructions == before && !cpu_idle {
                break;
            }
            guard += 1;
            if guard > TT_REPLAY_STEP_CAP {
                break;
            }
        }
        let _ = self.machine.take_ui_debug_stop();
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
        let addr = addr & self.machine.ui_addr_mask();
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
    /// query so the run continues unaffected. The retained history is not
    /// preserved: parking on the writing instruction discards the snapshots
    /// after it, so the ring restarts from the next capture. This is a
    /// one-shot diagnostic, so that costs the run nothing.
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
            ReverseOutcome::NotFound => {
                log::info!("DBG RWATCH ${addr:06X}: no write to it found in recorded history")
            }
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
        // A physical drive's platter turns in wall-clock time and cannot be
        // hurried, so a bridged machine stays paced whatever asks otherwise --
        // warp, the benchmark runner, the GDB stub, the control server. Left
        // unthrottled the guest outruns the drive: the motor is spun up and
        // down faster than it can reach speed, and tracks are stepped past
        // before the drive has captured them. Enforced here rather than at
        // each caller so no future runner can quietly opt out of it.
        #[cfg(feature = "fluxbridge")]
        if !paced && self.bus().floppy.has_bridged_drive() {
            return;
        }
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

    /// The always-on performance counters (see [`PerfCounters`]). Cleared,
    /// like the rest of the stats, by a guest reset.
    pub fn perf_counters(&self) -> PerfCounters {
        PerfCounters {
            busy: self.stats.busy,
            pacer_slips: self.stats.pacer_slips,
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

    /// Execute one debugger-controlled step using the same STOP/idle handling
    /// as the real-time loop. The caller owns `cpu_idle` across repeated calls
    /// so a CPU halted in STOP can advance devices to the next wake-up event
    /// without spinning on zero-instruction slices.
    pub fn debug_step_for_gdb(&mut self, cpu_idle: &mut bool) -> Result<()> {
        if self.stats.started_at.is_none() {
            self.stats.started_at = Some(crate::timebase::Instant::now());
        }
        let frame_before = self.bus().emulated_frames();
        self.run_one_step(cpu_idle, INSTRUCTIONS_PER_REALTIME_SLICE)?;
        let frame_after = self.bus().emulated_frames();
        if frame_after != frame_before {
            self.stats.frames = self
                .stats
                .frames
                .saturating_add(frame_after.saturating_sub(frame_before));
            self.tt_capture_if_due()?;
            self.tt_poll_reverse_watch()?;
        }
        Ok(())
    }

    /// Execute one control/debug slice as part of the same real-time quantum
    /// used by [`Self::step_frame`]. An exact mid-quantum stop leaves the
    /// unused budget here, so a later resume cannot move hardware events by
    /// changing an otherwise invisible host scheduling boundary.
    pub fn debug_step_realtime(&mut self) -> Result<()> {
        if self.stats.started_at.is_none() {
            self.stats.started_at = Some(crate::timebase::Instant::now());
        }
        let frame_before = self.bus().emulated_frames();
        let mut remaining = self.realtime_quantum_remaining;
        let mut cpu_idle = self.realtime_quantum_cpu_idle;
        if remaining == 0 {
            remaining = self.instruction_budget();
            cpu_idle = false;
        }
        let accounting = self.run_one_precise_step(&mut cpu_idle, remaining)?;
        remaining = remaining.saturating_sub(accounting.budget_debit);
        self.realtime_quantum_remaining = remaining;
        self.realtime_quantum_cpu_idle = remaining != 0 && cpu_idle;
        if remaining == 0 {
            // `stats.frames` counts completed host execution quanta. A
            // fine-grained stop may cross an emulated video frame without
            // completing this quantum; counting that crossing here and the
            // resumed remainder in `step_frame` would make observation alter
            // the reported frame rate.
            self.stats.frames = self.stats.frames.saturating_add(1);
        }

        let frame_after = self.bus().emulated_frames();
        if frame_after != frame_before {
            self.tt_capture_if_due()?;
            self.tt_poll_reverse_watch()?;
        }
        Ok(())
    }

    /// Execute one instruction with the same STOP/idle fast-forward handling as
    /// the real-time loop: a CPU halted in STOP makes no progress under a
    /// single-instruction slice, so advance devices to the next event and let
    /// the wake-up interrupt fire. Shared by the run-to / step-over / step-out
    /// helpers so they all step a STOPped CPU forward instead of spinning.
    fn debug_step_one_with_idle(&mut self) -> Result<()> {
        self.debug_step_realtime()
    }

    /// Run until the CPU reaches `target_pc` (masked to the bus width), up
    /// to `max_instructions`. Returns true when the target was hit.
    pub fn debug_run_to_pc(&mut self, target_pc: u32, max_instructions: usize) -> Result<bool> {
        let pc_mask = self.machine.ui_addr_mask();
        let target = target_pc & pc_mask;
        for _ in 0..max_instructions {
            self.debug_step_one_with_idle()?;
            if self.machine.pc() & pc_mask == target {
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

    /// Run until the program counter leaves an inclusive address window.
    /// This is the debugger's return-from-ROM primitive.
    pub fn debug_run_until_pc_outside(
        &mut self,
        lo: u32,
        hi: u32,
        max_instructions: usize,
    ) -> Result<bool> {
        let mask = self.machine.ui_addr_mask();
        for _ in 0..max_instructions {
            let pc = self.machine.pc() & mask;
            if !(lo..=hi).contains(&pc) {
                return Ok(true);
            }
            self.debug_step_one_with_idle()?;
            if self.machine.ui_debug_stop_pending() {
                return Ok(false);
            }
        }
        Ok(false)
    }

    /// Run until the Agnus beam reaches (`vpos`, `hpos`) -- `hpos: None`
    /// means the first colour clock of that line -- up to
    /// `max_instructions`. Arms a one-shot beam trap, so the stop lands at
    /// exact beam granularity (including while the CPU sits in STOP); an
    /// unrelated breakpoint/watch hit on the way ends the run with that
    /// stop pending instead. Returns true when a debug stop is pending
    /// (the beam trap or an earlier hit), false when the budget ran out.
    pub fn debug_run_to_beam(
        &mut self,
        vpos: u16,
        hpos: Option<u16>,
        max_instructions: usize,
    ) -> Result<bool> {
        self.bus_mut().ui_arm_beam_trap_once(vpos, hpos);
        for _ in 0..max_instructions {
            self.debug_step_one_with_idle()?;
            if self.machine.ui_debug_stop_pending() {
                return Ok(true);
            }
        }
        // Budget exhausted without reaching the position: disarm the
        // one-shot so it cannot fire out of nowhere later.
        self.bus_mut().ui_disarm_beam_trap_once();
        Ok(false)
    }

    /// Run until the Copper retires one instruction (a MOVE applied or
    /// skipped, a WAIT/SKIP/COPJMP started), up to `max_instructions` CPU
    /// instructions. The machine pauses at the next CPU instruction
    /// boundary after the Copper advances, so the Copper may already be a
    /// fetch further along -- its live PC shows exactly where it got to.
    /// Returns true when the Copper advanced (or an earlier debug stop is
    /// pending), false when the budget ran out (Copper stopped/DMA off).
    pub fn debug_step_copper(&mut self, max_instructions: usize) -> Result<bool> {
        let target = self.bus().copper_instructions_retired().wrapping_add(1);
        for _ in 0..max_instructions {
            self.debug_step_one_with_idle()?;
            if self.bus().copper_instructions_retired() >= target {
                return Ok(true);
            }
            if self.machine.ui_debug_stop_pending() {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Step over the instruction at PC. When it is a call that returns to the
    /// following instruction (BSR/JSR/TRAP), run until that return address (or
    /// an earlier breakpoint/watch hit, or the `max_instructions` budget if the
    /// call never returns); otherwise this is a plain single step.
    pub fn debug_step_over(&mut self, max_instructions: usize) -> Result<()> {
        let pc = self.machine.pc();
        let op = self.machine.bus().peek_word_any(pc);
        if !instruction_returns_inline(op) {
            return self.debug_step_instructions(1);
        }
        let cpu_type = self.machine.cpu_type();
        let len = {
            let bus = self.machine.bus();
            crate::disasm::disassemble(|a| bus.peek_word_any(a), pc, cpu_type).1
        };
        self.debug_run_to_pc(pc.wrapping_add(len), max_instructions)?;
        Ok(())
    }

    /// Run until the current subroutine returns to its caller, up to
    /// `max_instructions`. The return is detected by the stack pointer rising
    /// above its value at entry right after a return instruction (RTS/RTR/RTE/
    /// RTD): nested calls and interrupt handlers push below the entry frame and
    /// pop back to it, so only this frame's own return lifts the SP past entry.
    /// An earlier breakpoint/watch hit also ends the run.
    pub fn debug_step_out(&mut self, max_instructions: usize) -> Result<()> {
        let start_sp = self.machine.a(7);
        for _ in 0..max_instructions {
            let op = self.machine.bus().peek_word_any(self.machine.pc());
            let is_return = instruction_is_return(op);
            self.debug_step_one_with_idle()?;
            if is_return && self.machine.a(7) > start_sp {
                return Ok(());
            }
            if self.machine.ui_debug_stop_pending() {
                return Ok(());
            }
        }
        Ok(())
    }

    pub fn step_frame(&mut self) -> Result<()> {
        self.step_frame_with_pc_outside(None).map(|_| ())
    }

    /// Advance to the next hardware video-frame boundary. Frontends whose
    /// caller owns pacing (such as libretro) use this with an unpaced machine;
    /// `step_frame` instead advances a desktop CPU-budget quantum.
    pub fn step_video_frame(&mut self) -> Result<()> {
        self.step_netplay_frame(false)
    }

    /// Advance to the next hardware video frame for netplay. The normal
    /// frontend quantum is a CPU budget and can end within a raster frame;
    /// network inputs need a boundary defined entirely by the guest hardware.
    pub(crate) fn step_netplay_frame(&mut self, replay: bool) -> Result<()> {
        self.real_pacing_budget_mode = RealPacingBudgetMode::M68kCycles;
        self.reset_realtime_quantum();
        self.set_runahead_speculative(replay);
        let started = Instant::now();
        self.stats.started_at.get_or_insert(started);
        let frame = self.bus().emulated_frames();
        let mut idle = false;
        let result = (|| {
            while self.bus().emulated_frames() == frame {
                self.run_one_precise_step(&mut idle, INSTRUCTIONS_PER_REALTIME_SLICE)?;
                if self.machine.ui_debug_stop_pending() {
                    bail!("debugger stopped the netplay machine");
                }
            }
            Ok(())
        })();
        self.set_runahead_speculative(false);
        result?;
        let slept = if self.paced && !replay {
            self.sleep_until_realtime_device_time()
        } else {
            Duration::ZERO
        };
        self.stats.busy += started.elapsed().saturating_sub(slept);
        if !replay {
            self.stats.frames += 1;
        }
        Ok(())
    }

    /// Execute one normal window quantum, but stop on the first instruction
    /// boundary outside `lo..=hi`. This retains the same pacing, time-travel,
    /// profiling, and partial-quantum bookkeeping as [`Self::step_frame`].
    pub fn step_frame_until_pc_outside(&mut self, lo: u32, hi: u32) -> Result<bool> {
        self.step_frame_with_pc_outside(Some((lo, hi)))
    }

    fn step_frame_with_pc_outside(&mut self, pc_outside: Option<(u32, u32)>) -> Result<bool> {
        let outside = |machine: &cpu::M68kMachine| {
            pc_outside.is_some_and(|(lo, hi)| {
                let pc = machine.pc() & machine.ui_addr_mask();
                !(lo..=hi).contains(&pc)
            })
        };
        // A run request may already be satisfied when it reaches the window
        // thread. Do not manufacture an execution/statistics quantum for it.
        if outside(&self.machine) {
            return Ok(true);
        }
        if self.stats.started_at.is_none() {
            self.stats.started_at = Some(crate::timebase::Instant::now());
        }
        let reached = self.step_real_until(pc_outside)?;
        if !self.runahead_speculative {
            self.stats.frames += 1;
        }
        // Capture a reverse-debug snapshot at this frame boundary when one is
        // due (no-op unless reverse mode is armed). Frame boundaries are the
        // only safe capture points -- mid-frame the renderer capture buffers
        // are inconsistent (see M68kMachine::write_state).
        self.tt_capture_if_due()?;
        // Evaluate a time-targeted reverse watchpoint when its target is
        // reached (no-op unless armed).
        self.tt_poll_reverse_watch()?;
        // Feed a running profile capture from committed frames only.
        #[cfg(feature = "control")]
        if !self.runahead_speculative {
            self.profile_poll()?;
        }
        if !self.runahead_speculative
            && crate::envcfg::flag("COPPERLINE_DIAG_PCSAMPLE")
            && self.stats.frames.is_multiple_of(50)
        {
            log::info!(
                "pcsample frame={} pc={:#010X} sr={:#06X}",
                self.stats.frames,
                self.machine.pc(),
                self.machine.sr()
            );
        }
        Ok(reached)
    }

    fn step_real_until(&mut self, pc_outside: Option<(u32, u32)>) -> Result<bool> {
        // Host cost of this frame for the performance overlay: everything
        // in this call except the real-time pacing sleep.
        let busy_started = Instant::now();
        let mut remaining = self.realtime_quantum_remaining;
        let mut cpu_idle = self.realtime_quantum_cpu_idle;
        if remaining == 0 {
            remaining = self.instruction_budget();
            cpu_idle = false;
        }
        // Cycle-step while the CPU is actively executing so every chip
        // register write lands at the correct beam position (one
        // instruction per slice, with the chipset advanced for that
        // instruction immediately after it retires). While the CPU is
        // halted in STOP it writes nothing, so fast-forward to the next
        // device event instead of stepping one instruction at a time --
        // then drop straight back to single-instruction stepping so the
        // wake-up interrupt handler, which often performs mid-frame
        // display writes, is cycle-accurate too.
        let mut reached = false;
        while remaining > 0 {
            let accounting = if pc_outside.is_some() {
                self.run_one_precise_step(&mut cpu_idle, remaining)?
            } else {
                self.run_one_step(&mut cpu_idle, remaining)?
            };
            remaining = remaining.saturating_sub(accounting.budget_debit);
            if let Some((lo, hi)) = pc_outside {
                let pc = self.machine.pc() & self.machine.ui_addr_mask();
                if !(lo..=hi).contains(&pc) {
                    reached = true;
                    break;
                }
            }
            // An interactive breakpoint/watch hit ends the frame early;
            // the window surfaces it and pauses. (Checked after the device
            // advance so a hit during an idle fast-forward is seen too.)
            if self.machine.ui_debug_stop_pending() {
                break;
            }
        }
        self.realtime_quantum_remaining = remaining;
        self.realtime_quantum_cpu_idle = remaining != 0 && cpu_idle;
        // Pace presentation to wall-clock only for the interactive window;
        // headless runs advance the deterministic core unthrottled. During a
        // run-ahead burst the sleep is deferred to the burst's anchor target
        // (`pace_runahead_burst`), so speculative frames retire unpaced.
        let slept = if self.paced && !self.runahead_phase {
            self.sleep_until_realtime_device_time()
        } else {
            Duration::ZERO
        };
        self.stats.busy += busy_started.elapsed().saturating_sub(slept);
        Ok(reached)
    }

    fn reset_realtime_quantum(&mut self) {
        self.realtime_quantum_remaining = 0;
        self.realtime_quantum_cpu_idle = false;
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
        self.run_one_step_mode(cpu_idle, idle_cap, false)
    }

    /// Debugger-controlled stepping must not let the JIT retire a batch past
    /// an exact PC target. STOP-state fast-forwarding remains bounded exactly
    /// as in the ordinary real-time loop.
    fn run_one_precise_step(
        &mut self,
        cpu_idle: &mut bool,
        idle_cap: usize,
    ) -> Result<RealSliceAccounting> {
        self.run_one_step_mode(cpu_idle, idle_cap, true)
    }

    fn run_one_step_mode(
        &mut self,
        cpu_idle: &mut bool,
        idle_cap: usize,
        force_precise: bool,
    ) -> Result<RealSliceAccounting> {
        let chunk = if *cpu_idle {
            self.idle_fast_forward_chunk(idle_cap)
        } else if self.cpu_jit && !force_precise {
            // JIT mode hands the machine a fixed multi-instruction slice;
            // batching (and interrupt recognition) inside it is handled by
            // `step_slice_jit`. A fixed size -- never derived from the
            // caller's budget remainder -- keeps the batch boundaries, and
            // therefore interrupt delivery, identical for every caller that
            // replays the same machine state.
            INSTRUCTIONS_PER_REALTIME_SLICE
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
            self.bus_mut().advance_cpu_idle_devices(idle_cck);
        }
        // `refresh_irq_line` applies any deferred timed-device color clocks
        // before sampling the interrupt line (see its body), so a device
        // interrupt that came due during the slice is recognized here.
        self.machine.refresh_irq_line();
        self.real_pacing_profile.record_slice(&run, accounting);
        // Only a single-instruction slice that came back stopped tells us the
        // CPU is genuinely idle; never batch on the slice right after a
        // fast-forward, so a wake-up is always stepped. The JIT path runs
        // multi-instruction slices, so there a stopped slice that retired
        // nothing at all is the equivalent evidence -- without it the idle
        // fast-forward (whose nap is bounded to the next device event) never
        // engages and a STOPped guest free-runs past fine-grained device
        // timing in whole-slice blind naps.
        *cpu_idle =
            run.cpu_stopped && (chunk == 1 || (self.cpu_jit && run.actual_instructions == 0));
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
        // An interrupt already latched in INTREQ but still inside its
        // IPL-pin recognition pipe is invisible to the CPU right now, and a
        // halted CPU has nothing else to wake it: without this bound the nap
        // sleeps straight past the moment the pipe delivers, and the handler
        // is entered a whole nap late instead of a few colour clocks late.
        if let Some(cck) = bus.next_irq_visible_cck() {
            cap_cck(cck, &mut chunk);
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
        // A serial sink wired to a live host byte source (TCP, pty, the
        // browser channel) can start a reception at any wall-clock moment,
        // invisibly to the event horizon computed here. Bound the blind nap
        // while one is attached, so a byte landing mid-nap is recognized
        // within a fraction of a character time -- a real machine halted in
        // STOP wakes on RBF within microseconds, and sleeping past a whole
        // character would overrun the one-word receive buffer.
        if bus.paula.serial.can_produce_input() {
            cap_cck(SERIAL_LIVE_IDLE_CAP_CCK, &mut chunk);
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

    /// Returns the host time actually slept, so the caller can subtract it
    /// from the frame's busy-time accounting.
    fn sleep_until_realtime_device_time(&mut self) -> Duration {
        let emulated_now = self.bus().emulated_seconds();
        self.pace_to_emulated_target(emulated_now)
    }

    /// Sleep until wall-clock reaches `emulated_now` minus the live-audio
    /// lead. Shared by the per-frame pacer and the run-ahead burst pacer
    /// (which passes an anchor-frame target instead of the current time).
    fn pace_to_emulated_target(&mut self, emulated_now: f64) -> Duration {
        let Some(started_at) = self.stats.started_at else {
            return Duration::ZERO;
        };
        let mut slept = Duration::ZERO;
        let now = Instant::now();
        let live_audio_lead_seconds = self.bus().live_audio_output_lead_seconds();
        let target = realtime_device_time_target(started_at, emulated_now, live_audio_lead_seconds);
        if let Some(wait) = target.and_then(|target| target.checked_duration_since(now)) {
            let sleep_started = Instant::now();
            std::thread::sleep(wait);
            let elapsed = sleep_started.elapsed();
            self.audio_profile.record_sleep(elapsed);
            self.real_pacing_profile.record_sleep(elapsed);
            slept = elapsed;
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
                        self.stats.pacer_slips = self.stats.pacer_slips.saturating_add(1);
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
        slept
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

/// Metadata for a successful registered-resource PNG export.
pub struct ResourceExport {
    pub name: String,
    pub kind: &'static str,
    pub width: usize,
    pub height: usize,
    pub note: Option<String>,
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
        // A pure idle single step (no instruction retired) advances one
        // colour clock: a STOPped 68000 samples its IPL pins every bus-cycle
        // period, so the wake-up quantum near a device event is 2 CPU clocks,
        // not a whole instruction's worth of time.
        let device_cck = if run.actual_instructions == 0 && requested_instructions == 1 {
            run.actual_cpu_cck.max(1)
        } else if run.actual_instructions > 0 {
            // A multi-instruction (JIT) slice that ran and then hit STOP:
            // its billed time is the device time. Flooring at the requested
            // span would nap blindly past every device event by the rest of
            // the slice (thousands of instructions), inflating each Wait()'s
            // wake-up latency; the idle period belongs to the NEXT slice,
            // whose fast-forward is bounded to the next device event. The
            // precise path never reaches here (its stopping slice is a
            // single-instruction slice, accounted as running).
            run.actual_cpu_cck
        } else {
            run.actual_cpu_cck.max(cck_for_instructions(
                requested_instructions,
                cpu_cycles_per_instruction,
            ))
        };
        let budget_debit = if run.actual_instructions > 0 {
            run.actual_instructions
        } else {
            requested_instructions.max(1)
        };
        return RealSliceAccounting {
            budget_debit,
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

/// Build a fully-configured [`Emulator`] from a validated [`Config`]: the
/// Zorro autoconfig chain, RAM/ROM (and the A1000 bootstrap special case),
/// optional SCSI/IDE/CD controllers, floppy drives, Paula (with the supplied
/// audio sink), and the CPU with its caches and machine descriptor. Shared by
/// the command-line boot path in `main` and the configuration screen's Run
/// button, so a machine built either way is identical. `rom_optional` allows
/// a missing ROM file when a save state will supply the image.
/// Build the serial sink Paula writes SERDAT bytes through, per `[serial]`.
/// `Off` discards, `Stdout` is the historical terminal output, and `Midi`
/// bridges to host MIDI endpoints. `Midi` needs a `--features midi` build, so
/// without it a `midi` config is a clear error rather than a silent no-op.
fn build_serial_sink(cfg: &Config) -> Result<Box<dyn crate::serial::SerialSink>> {
    use crate::config::SerialMode;
    match cfg.serial.mode {
        SerialMode::Off => Ok(Box::new(crate::serial::NullSerialSink)),
        SerialMode::Stdout => Ok(Box::new(StdoutSink::new())),
        #[cfg(feature = "midi")]
        SerialMode::Midi => {
            // "mt32" names the built-in synth, not a host endpoint, so the
            // host output starts unset and the device is attached below.
            let wants_mt32 = crate::config::midi_out_is_mt32(cfg.serial.midi_out.as_deref());
            let wants_csynth = crate::config::midi_out_is_csynth(cfg.serial.midi_out.as_deref());
            let host_out = (!wants_mt32 && !wants_csynth)
                .then_some(cfg.serial.midi_out.as_deref())
                .flatten();
            // Likewise on the way in: "mt32" is the module's own MIDI OUT,
            // and it only has one while it is also the output. Either way
            // the name is a sentinel, never a host endpoint's, so it does
            // not reach the backend even when the module is not driving it.
            let names_mt32_in = crate::config::midi_out_is_mt32(cfg.serial.midi_in.as_deref());
            let wants_mt32_in = wants_mt32 && names_mt32_in;
            let host_in = (!names_mt32_in)
                .then_some(cfg.serial.midi_in.as_deref())
                .flatten();
            #[allow(unused_mut)]
            let mut sink = crate::midi::MidiSerialSink::open(host_out, host_in)?;
            #[cfg(feature = "mt32")]
            {
                let (control_override, pcm_override) = crate::mt32::rom_overrides();
                sink.set_mt32_roms(crate::mt32::Mt32Roms {
                    control: control_override.or_else(|| cfg.serial.mt32_control_rom.clone()),
                    pcm: pcm_override.or_else(|| cfg.serial.mt32_pcm_rom.clone()),
                });
                if wants_mt32 {
                    sink.set_output_endpoint(Some(crate::config::MIDI_OUT_MT32));
                }
                if wants_mt32_in {
                    sink.set_input_endpoint(Some(crate::config::MIDI_OUT_MT32));
                }
            }
            #[cfg(feature = "coppersynth")]
            {
                sink.set_csynth_options(crate::csynth::CsynthOptions {
                    soundfont: cfg.serial.coppersynth_soundfont.clone(),
                    mt32_mode: cfg.serial.coppersynth_mt32_mode.clone(),
                });
                if wants_csynth {
                    sink.set_output_endpoint(Some(crate::config::MIDI_OUT_CSYNTH));
                }
            }
            #[cfg(not(feature = "coppersynth"))]
            if wants_csynth {
                log::warn!(
                    "[serial] midi_out = \"coppersynth\" needs a build with --features coppersynth; \
                     the MIDI output is unset"
                );
            }
            // Coppersynth has no MIDI OUT jack of its own, so there is
            // nothing to wire back as an input.
            if wants_csynth && crate::config::midi_out_is_csynth(cfg.serial.midi_in.as_deref()) {
                log::warn!(
                    "[serial] midi_in = \"coppersynth\": Coppersynth has no MIDI \
                     output to read back; the MIDI input is unset"
                );
            }
            #[cfg(not(feature = "mt32"))]
            let _ = wants_mt32_in;
            #[cfg(not(feature = "mt32"))]
            if wants_mt32 {
                log::warn!(
                    "[serial] midi_out = \"mt32\" needs a build with --features mt32; \
                     the MIDI output is unset"
                );
            }
            if names_mt32_in && !wants_mt32_in {
                log::warn!(
                    "[serial] midi_in = \"mt32\" needs midi_out = \"mt32\" as well: \
                     the module answers what it is sent, so with nothing going \
                     to it there is nothing to hear back; the MIDI input is unset"
                );
            }
            sink.report_wiring();
            Ok(Box::new(sink))
        }
        #[cfg(not(feature = "midi"))]
        SerialMode::Midi => Err(anyhow!(
            "[serial] mode = \"midi\" needs a build with --features midi"
        )),
        SerialMode::Tcp => Ok(Box::new(crate::serial::TcpSerialSink::listen(
            cfg.serial
                .listen
                .as_deref()
                .unwrap_or(crate::config::SERIAL_TCP_DEFAULT_LISTEN),
        )?)),
        SerialMode::TcpConnect => {
            let addr = cfg.serial.connect.as_deref().ok_or_else(|| {
                anyhow!(
                    "[serial] mode = \"tcp-connect\" needs a remote address: set \
                     [serial] connect = \"host:port\", pass --serial-connect, \
                     or fill in the launcher's I/O Ports > Serial > Connect box"
                )
            })?;
            Ok(Box::new(crate::serial::TcpSerialSink::connect(addr)?))
        }
        #[cfg(unix)]
        SerialMode::Pty => Ok(Box::new(crate::serial::PtySerialSink::open()?)),
        #[cfg(not(unix))]
        SerialMode::Pty => Err(anyhow!(
            "[serial] mode = \"pty\" is only available on Unix hosts"
        )),
        SerialMode::Modem => {
            let options = crate::modem::ModemOptions {
                listen: cfg.serial.listen.clone(),
                telnet: cfg.serial.telnet,
                phonebook: cfg.serial.phonebook.clone(),
            };
            match cfg.serial.session.as_deref() {
                Some(path) => Ok(Box::new(crate::modem::ModemSerialSink::new_scripted(
                    path, options,
                )?)),
                None => Ok(Box::new(crate::modem::ModemSerialSink::new_tcp(options)?)),
            }
        }
    }
}

/// Attach every real host disk the config puts on an IDE position, whichever
/// interface the machine has -- Gayle's or the A4000's.
///
/// A configuration outlives the card reader it was written at, so a disk that
/// is not here is a thing to report and carry on from: the machine boots with
/// that slot empty, as it would if the drive had been unplugged. Only a disk
/// that is present is opened, so a missing one never raises the host's
/// permission prompt.
#[cfg(not(target_arch = "wasm32"))]
fn attach_ide_host_disks(cfg: &Config, mut attach: impl FnMut(usize, crate::ata::IdeDrive)) {
    for disk in &cfg.host_disks {
        // SCSI units and lide positions are attached elsewhere.
        let slot = match disk.attach {
            crate::config::HostDiskAttach::IdeMaster => 0,
            crate::config::HostDiskAttach::IdeSlave => 1,
            crate::config::HostDiskAttach::LideMaster(_)
            | crate::config::HostDiskAttach::LideSlave(_)
            | crate::config::HostDiskAttach::Scsi(_) => continue,
        };
        match crate::ata::IdeDrive::open_host_disk(
            &disk.device,
            disk.fingerprint.as_deref(),
            disk.identity_confirmed,
            disk.writable,
        ) {
            Ok(drive) => {
                attach(slot, drive);
                info!(
                    "ide: {} is host disk {}{}",
                    disk.attach.label(),
                    disk.device,
                    if disk.writable {
                        " (WRITABLE)"
                    } else {
                        " (read-only)"
                    }
                );
            }
            Err(error) => warn!(
                "ide: {} asked for host disk {}, which is not available: {error}",
                disk.attach.label(),
                disk.device
            ),
        }
    }
}

/// Attach every real host disk the config puts on a `[lide]` channel.
///
/// A configuration outlives the disk it names, so one that is not here is
/// reported and skipped: the machine comes up with that position empty, as
/// it would if the drive had been unplugged. Only a disk that is present is
/// opened, so a missing one never raises the host's permission prompt.
#[cfg(not(target_arch = "wasm32"))]
fn attach_lide_host_disks(
    cfg: &Config,
    mut attach: impl FnMut(usize, usize, crate::ata::IdeDrive),
) {
    for disk in &cfg.host_disks {
        let (channel, slot) = match disk.attach {
            crate::config::HostDiskAttach::LideMaster(ch) => (usize::from(ch), 0),
            crate::config::HostDiskAttach::LideSlave(ch) => (usize::from(ch), 1),
            _ => continue,
        };
        match crate::ata::IdeDrive::open_host_disk(
            &disk.device,
            disk.fingerprint.as_deref(),
            disk.identity_confirmed,
            disk.writable,
        ) {
            Ok(drive) => {
                attach(channel, slot, drive);
                info!(
                    "lide: {} is host disk {}{}",
                    disk.attach.label(),
                    disk.device,
                    if disk.writable {
                        " (WRITABLE)"
                    } else {
                        " (read-only)"
                    }
                );
            }
            Err(error) => warn!(
                "lide: {} asked for host disk {}, which is not available: {error}",
                disk.attach.label(),
                disk.device
            ),
        }
    }
}

/// Attach every real host disk the config puts on a SCSI unit.
///
/// A configuration outlives the disk it names, so one that is not here is
/// reported and skipped: the machine comes up with that unit empty, as it
/// would if the drive had been unplugged. Only a disk that is present is
/// opened, so a missing one never raises the host's permission prompt.
#[cfg(not(target_arch = "wasm32"))]
fn attach_scsi_host_disks(
    cfg: &Config,
    mut attach: impl FnMut(usize, crate::scsi::ScsiTarget),
) -> usize {
    let mut attached = 0;
    for disk in &cfg.host_disks {
        let crate::config::HostDiskAttach::Scsi(unit) = disk.attach else {
            continue;
        };
        let unit = usize::from(unit);
        match crate::scsi::ScsiDisk::open_host_disk(
            &disk.device,
            disk.fingerprint.as_deref(),
            disk.identity_confirmed,
            disk.writable,
        ) {
            Ok(drive) => {
                attach(unit, drive.into());
                attached += 1;
                info!(
                    "scsi: unit {unit} is host disk {}{}",
                    disk.device,
                    if disk.writable {
                        " (WRITABLE)"
                    } else {
                        " (read-only)"
                    }
                );
            }
            Err(error) => warn!(
                "scsi: unit {unit} asked for host disk {}, which is not available: {error}",
                disk.device
            ),
        }
    }
    attached
}

fn open_scsi_target(
    drive: &crate::config::DriveImage,
    unit: usize,
) -> Result<crate::scsi::ScsiTarget> {
    if crate::config::is_cd_image_path(&drive.path) {
        let cd = crate::scsi::ScsiCdRom::open(&drive.path)?;
        info!(
            "scsi: unit {unit} CD-ROM {} ({})",
            drive.path.display(),
            cd.describe()
        );
        Ok(cd.into())
    } else {
        let disk = crate::scsi::ScsiDisk::open(
            &drive.path,
            unit,
            drive.volume_name.as_deref(),
            drive.boot_pri,
            drive.filesystem,
        )?;
        info!("scsi: unit {unit} {}", drive.path.display());
        Ok(disk.into())
    }
}

/// Open an `[ide]`/`[lide]` drive slot: a CD image attaches as ATAPI, exactly
/// as `open_scsi_target` attaches one as a SCSI CD-ROM.
fn open_ide_target(
    path: &std::path::Path,
    unit: usize,
    volume_name: Option<&str>,
    boot_pri: i8,
    filesystem: crate::diskimage::FileSystem,
) -> Result<crate::ata::AtaDevice> {
    if crate::config::is_cd_image_path(path) {
        let cd = crate::ata::AtapiDrive::open(path)?;
        Ok(cd.into())
    } else {
        let disk = crate::ata::IdeDrive::open(path, unit, volume_name, boot_pri, filesystem)?;
        Ok(disk.into())
    }
}

/// Open every drive the config bridges to real hardware.
///
/// Failing to open one is fatal rather than a warning: a bay configured as a
/// real drive has no image to fall back on, so carrying on would silently boot
/// a machine with an empty drive where the user asked for their disk.
#[cfg(feature = "fluxbridge")]
pub(crate) fn attach_floppy_bridges(floppy: &mut FloppyController, cfg: &Config) -> Result<()> {
    use crate::config::{BridgeCable, BridgeDensity, BridgeReadMode};
    use crate::fluxbridge::{
        self, Bridge, BridgeConfig, BridgeDensityMode, BridgeMode, DriveSelection,
    };

    for (idx, bridge_cfg) in cfg.floppy.bridges.iter().enumerate() {
        let Some(bridge_cfg) = bridge_cfg else {
            continue;
        };
        // The bridge is compiled into this binary, so it cannot be missing --
        // the link would have failed. This is a failsafe against a build with
        // every FluxBridge driver feature turned off, which would leave the
        // library linked in but offering nothing.
        if fluxbridge::drivers().is_empty() {
            anyhow::bail!(
                "floppy.df{idx} asks for a physical drive, but this build of Copperline \
                 compiled no FluxBridge drivers in"
            );
        }
        // Resolve the driver by the library's own token, so the config does
        // not depend on enumeration order or on name spellings kept in step
        // by hand.
        let driver =
            fluxbridge::driver_named(bridge_cfg.driver.match_token()).ok_or_else(|| {
                anyhow!(
                    "floppy.df{idx}: this build of Copperline has no {} driver",
                    bridge_cfg.driver.label()
                )
            })?;
        let open = BridgeConfig {
            driver: driver.index,
            mode: match bridge_cfg.mode {
                BridgeReadMode::Compatible => BridgeMode::Compatible,
                BridgeReadMode::Normal => BridgeMode::Normal,
                BridgeReadMode::Stalling => BridgeMode::Stalling,
            },
            density: match bridge_cfg.density {
                BridgeDensity::Auto => BridgeDensityMode::Auto,
                BridgeDensity::Dd => BridgeDensityMode::DdOnly,
                BridgeDensity::Hd => BridgeDensityMode::HdOnly,
            },
            drive: match bridge_cfg.cable {
                BridgeCable::DriveA => DriveSelection::DriveA,
                BridgeCable::DriveB => DriveSelection::DriveB,
                BridgeCable::Shugart0 => DriveSelection::Drive0,
                BridgeCable::Shugart1 => DriveSelection::Drive1,
                BridgeCable::Shugart2 => DriveSelection::Drive2,
                BridgeCable::Shugart3 => DriveSelection::Drive3,
            },
            port: bridge_cfg.port.clone(),
        };
        let bridge = Bridge::open(&open)
            .map_err(|e| anyhow!("floppy.df{idx}: could not open the physical drive: {e}"))?;
        // Name the port as well as the interface: with auto-detect on, the
        // library does not report back which one it took, so say so from the
        // ports it can see -- unambiguous when there is only one, and honest
        // rather than guessing when there is not.
        let port = match bridge_cfg.port.as_deref() {
            Some(port) => port.to_string(),
            None => {
                let seen = fluxbridge::com_ports();
                match seen.len() {
                    1 => format!("{} (auto-detected)", seen[0]),
                    _ => "auto-detected".to_string(),
                }
            }
        };
        let drive_type = match bridge.drive_type() {
            fluxbridge::DriveType::Dd35 => "3.5\" DD",
            fluxbridge::DriveType::Dd35Hd => "3.5\" HD",
            fluxbridge::DriveType::Sd525 => "5.25\" SD",
        };
        log::info!(
            "floppy.df{idx} physical drive attached: {} on {port}, {drive_type} drive, FluxBridge v{}",
            bridge_cfg.driver.label(),
            fluxbridge::version(),
        );
        // Whether there is anything in it is the next thing anyone wants to
        // know, and unlike an image nobody told us either way.
        if bridge.disk_in_drive() {
            log::info!("floppy.df{idx} disk in the physical drive");
        } else {
            log::info!("floppy.df{idx} no disk in the physical drive");
        }
        if bridge_cfg.write_protected {
            log::info!(
                "floppy.df{idx} write-protected by the configuration; \
                 set write_protected = false to write to the disk"
            );
        }
        floppy.attach_bridge(idx, bridge, bridge_cfg.write_protected, bridge_cfg.speed)?;
    }
    Ok(())
}

pub fn build_machine(
    cfg: &Config,
    audio: Box<dyn AudioSink>,
    paced: bool,
    rom_optional: bool,
) -> Result<Emulator> {
    build_machine_inner(cfg, audio, paced, rom_optional, cfg.netplay_storage)
}

/// Build a cold machine whose hard-drive writes belong only to this session.
/// Callers validate the config and supply local, staged media paths.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn build_netplay_machine(
    cfg: &Config,
    audio: Box<dyn AudioSink>,
    paced: bool,
) -> Result<Emulator> {
    build_machine_inner(cfg, audio, paced, false, true)
}

fn build_machine_inner(
    cfg: &Config,
    audio: Box<dyn AudioSink>,
    paced: bool,
    rom_optional: bool,
    netplay: bool,
) -> Result<Emulator> {
    let open_disk = |path: &std::path::Path,
                     name: &str,
                     bus: &'static str,
                     volume: Option<&str>,
                     priority: i8,
                     filesystem: crate::diskimage::FileSystem| {
        if netplay {
            let mut disk = crate::harddrive::HardDriveImage::open_session(
                path, name, bus, volume, priority, filesystem,
            )?;
            disk.set_session_read_only(cfg.netplay_read_only.iter().any(|p| p == path));
            Ok(disk)
        } else {
            crate::harddrive::HardDriveImage::open(path, name, bus, volume, priority, filesystem)
        }
    };
    let open_ide_target = |path: &std::path::Path,
                           unit: usize,
                           volume: Option<&str>,
                           priority: i8,
                           filesystem: crate::diskimage::FileSystem| {
        if netplay {
            anyhow::ensure!(
                !crate::config::is_cd_image_path(path),
                "netplay cannot use ATAPI CD images"
            );
            let disk = open_disk(
                path,
                &format!("DH{unit}"),
                "ide",
                volume,
                priority,
                filesystem,
            )?;
            Ok(crate::ata::AtaDevice::from(
                crate::ata::IdeDrive::from_disk(disk),
            ))
        } else {
            open_ide_target(path, unit, volume, priority, filesystem)
        }
    };
    let open_scsi_target = |drive: &crate::config::DriveImage, unit| {
        if netplay {
            anyhow::ensure!(
                !crate::config::is_cd_image_path(&drive.path),
                "netplay cannot use SCSI CD images"
            );
            let disk = open_disk(
                &drive.path,
                &format!("DH{unit}"),
                "scsi",
                drive.volume_name.as_deref(),
                drive.boot_pri,
                drive.filesystem,
            )?;
            Ok(crate::scsi::ScsiTarget::from(
                crate::scsi::ScsiDisk::from_disk(disk),
            ))
        } else {
            open_scsi_target(drive, unit)
        }
    };
    let mut zorro = cfg.build_zorro_chain()?;
    // Functional Zorro-chain boards. Each board's autoconfig identity goes on
    // the chain (mapping its window to a device slot) while the device object
    // is attached to the bus after it is built; the slot index ties them.
    let mut devices: Vec<crate::zorro_device::BoardDevice> = Vec::new();
    // The CD32 FMV cartridge must be first in the chain: its production ROM
    // and driver expect expansion.library's first 1 MiB Zorro II placement,
    // $200000.  Prepending keeps that physical address even when the config
    // also names fast RAM or other expansion boards.
    #[cfg(feature = "cd32-fmv")]
    if let Some(rom_path) = &cfg.fmv_rom_path {
        let slot = devices.len();
        let rom = if rom_optional && !rom_path.is_file() {
            info!(
                "--load-state: CD32 FMV ROM {} is unavailable; building with a \
                 placeholder the save state will replace",
                rom_path.display()
            );
            vec![0u8; crate::cd32_fmv::ROM_BYTES]
        } else {
            std::fs::read(rom_path)
                .with_context(|| format!("reading CD32 FMV ROM {}", rom_path.display()))?
        };
        let board = crate::cd32_fmv::Cd32Fmv::new(rom)?;
        zorro.prepend_board(crate::zorro::BoardSpec::cd32_fmv(slot))?;
        info!(
            "cd32-fmv: Full Motion Video cartridge first on the Zorro chain \
             (slot {slot}), ROM {}",
            rom_path.display()
        );
        devices.push(crate::zorro_device::BoardDevice::Cd32Fmv(Box::new(board)));
    }
    #[cfg(not(feature = "cd32-fmv"))]
    if cfg.fmv_rom_path.is_some() {
        anyhow::bail!("fmv_rom needs a build with the cd32-fmv feature");
    }
    // The A3000's motherboard SCSI is not a Zorro board: its drives are fitted
    // to the Super DMAC further down, once the bus exists.
    if cfg.scsi.enabled() && cfg.scsi.controller.is_zorro_board() {
        use crate::config::ScsiController;
        let rom_path = cfg.scsi.rom.as_ref().expect("config validated [scsi] rom");
        let slot = devices.len();
        // The controller picks the board; the drive plumbing is identical.
        let device = match cfg.scsi.controller {
            ScsiController::A3000 => unreachable!("not a Zorro board"),
            ScsiController::A2091 => {
                let rom = crate::a2091::A2091::load_rom(rom_path, cfg.scsi.rom_odd.as_deref())?;
                let mut board = crate::a2091::A2091::new(rom)?;
                for (unit, drive) in cfg.scsi.units.iter().enumerate() {
                    let Some(drive) = drive else { continue };
                    board.attach_drive(unit, open_scsi_target(drive, unit)?);
                }
                #[cfg(not(target_arch = "wasm32"))]
                attach_scsi_host_disks(cfg, |unit, target| board.attach_drive(unit, target));
                zorro.add_board(crate::zorro::BoardSpec::a2091(slot))?;
                info!(
                    "scsi: A2091 controller on the Zorro chain (slot {slot}), ROM {}",
                    rom_path.display()
                );
                crate::zorro_device::BoardDevice::A2091(board)
            }
            ScsiController::A4091 => {
                // A save state carries the board's ROM image (a4091::A4091.rom
                // is serialized), so an unavailable ROM here -- a missing file,
                // or the still-unresolved <bundled-a4091> sentinel when the
                // bundled ROM was not found -- is fine in --load-state mode:
                // build with a placeholder the state replaces, as the main ROM
                // does above.
                let rom = if rom_optional && !rom_path.is_file() {
                    info!(
                        "--load-state: A4091 ROM {} is unavailable; building with \
                         a placeholder the save state will replace",
                        rom_path.display()
                    );
                    vec![0u8; 0x1_0000]
                } else {
                    crate::a4091::A4091::load_rom(rom_path)?
                };
                let mut board = crate::a4091::A4091::new(rom)?;
                for (unit, drive) in cfg.scsi.units.iter().enumerate() {
                    let Some(drive) = drive else { continue };
                    board.attach_drive(unit, open_scsi_target(drive, unit)?);
                }
                #[cfg(not(target_arch = "wasm32"))]
                attach_scsi_host_disks(cfg, |unit, target| board.attach_drive(unit, target));
                zorro.add_board(crate::zorro::BoardSpec::a4091(slot))?;
                info!(
                    "scsi: A4091 controller on the Zorro chain (slot {slot}), ROM {}",
                    rom_path.display()
                );
                crate::zorro_device::BoardDevice::A4091(board)
            }
        };
        devices.push(device);
    }
    // A lide.device-compatible Zorro II IDE board (`[lide]`): RIPPLE, RIDE,
    // or AT-Bus 2008. Drives may be hard disks or ATAPI CD-ROMs; a fitted
    // board with no rom named defaults to the bundled ROM for its
    // personality (resolve_bundled_rom), and rom = "" is a legal
    // hardware-only mode.
    if cfg.lide.enabled() {
        let slot = devices.len();
        let has_rom = cfg.lide.rom.is_some();
        let mut flash = Vec::new();
        if let Some(rom_path) = &cfg.lide.rom {
            // Same --load-state placeholder handling as the A4091: the flash
            // is serialized into save states, so a ROM that is temporarily
            // unavailable while resuming is fine -- the state replaces it.
            if rom_optional && !rom_path.is_file() {
                info!(
                    "--load-state: lide ROM {} is unavailable; building with \
                     a placeholder the save state will replace",
                    rom_path.display()
                );
            } else {
                flash = crate::ide_zorro::IdeZorro::load_rom(rom_path)?;
                if let Some(bank2_path) = &cfg.lide.rom_bank2 {
                    flash.extend(crate::ide_zorro::IdeZorro::load_rom(bank2_path)?);
                }
            }
        }
        let mut board = crate::ide_zorro::IdeZorro::new(cfg.lide.board, flash)?;
        let channels = cfg.lide.board.channels();
        for (idx, drive) in cfg.lide.drives.iter().enumerate() {
            let Some(drive) = drive else { continue };
            let (ch, unit) = (idx / 2, idx % 2);
            if ch >= channels {
                continue; // config validation already rejects this; defensive only
            }
            // `unit` (master/slave, 0/1) is channel-relative and must stay
            // that way for the attach slot below, but a bare-partition
            // hardfile's synthesized RDB names its DOS device from this same
            // argument (`open_ide_target` -> `IdeDrive::open`) -- reusing
            // `unit` there would give every channel's master DH0 and every
            // channel's slave DH1, colliding across channels on RIPPLE's two
            // channels. `idx` (the flat 0..4 slot index) is unique per drive
            // regardless of channel, so it -- not `unit` -- is what goes into
            // the name.
            let target = open_ide_target(
                &drive.path,
                idx,
                drive.volume_name.as_deref(),
                drive.boot_pri,
                drive.filesystem,
            )?;
            board.attach_drive(ch, unit, target);
        }
        #[cfg(not(target_arch = "wasm32"))]
        attach_lide_host_disks(cfg, |ch, unit, drive| {
            if ch >= channels {
                return; // config validation already rejects this; defensive only
            }
            board.attach_drive(ch, unit, drive);
        });
        zorro.add_board(crate::zorro::BoardSpec::lide(cfg.lide.board, slot, has_rom))?;
        info!(
            "lide: {} controller on the Zorro chain (slot {slot}){}",
            cfg.lide.board.name(),
            cfg.lide
                .rom
                .as_ref()
                .map(|p| format!(", ROM {}", p.display()))
                .unwrap_or_default()
        );
        devices.push(crate::zorro_device::BoardDevice::IdeZorro(board));
    }
    // copperhf.device virtual hardfile controller (`[copperhf]`): up to
    // seven units, each opened exactly like an `[ide]`/`[scsi]` unit (same
    // `DriveImage` shape, same `DH{unit}` naming convention) so a bare
    // hardfile boots identically regardless of which controller it is
    // attached to. Failing to open a configured unit's image is fatal, not
    // a warning, matching the IDE/SCSI attach blocks above: a unit named in
    // the config has no image to fall back on.
    if cfg.copperhf.enabled() {
        let slot = devices.len();
        let mut board = crate::copperhf::CopperhfBoard::new();
        for (unit, drive) in cfg.copperhf.units.iter().enumerate() {
            let Some(drive) = drive else { continue };
            let disk = open_disk(
                &drive.path,
                &format!("DH{unit}"),
                "copperhf",
                drive.volume_name.as_deref(),
                drive.boot_pri,
                drive.filesystem,
            )?;
            board.attach_unit(unit, disk);
        }
        zorro.add_board(crate::zorro::BoardSpec::copperhf(slot))?;
        info!("copperhf: virtual hardfile controller on the Zorro chain (slot {slot})");
        devices.push(crate::zorro_device::BoardDevice::Copperhf(board));
    }
    // WASM plugin boards: assign each a device slot, put its autoconfig
    // identity on the chain, and instantiate the module.
    #[cfg(feature = "wasm-boards")]
    for wb in &cfg.wasm_boards {
        let slot = devices.len();
        let mut spec = wb.spec.clone();
        spec.backing = crate::zorro::BoardBacking::Device(slot);
        zorro.add_board(spec)?;
        let board = crate::wasmboard::WasmBoard::from_file(&wb.wasm_path, wb.manifest.clone())?;
        info!(
            "zorro: WASM plugin {:?} on the Zorro chain (slot {slot}), module {}",
            wb.manifest.name,
            wb.wasm_path.display()
        );
        devices.push(crate::zorro_device::BoardDevice::Wasm(board));
    }
    #[cfg(not(feature = "wasm-boards"))]
    if !cfg.wasm_boards.is_empty() {
        anyhow::bail!(
            "[[zorro]] wasm boards and [hostsocket] require a build with the wasm-boards feature"
        );
    }
    // Copperline services board (`[[filesys]]`): the guest-side handler ROM,
    // mount table, and per-unit host register banks in one 64K window; see
    // crate::filesys. The scsi.device cull rides the same DiagPoint, so the
    // board is also fitted (with no mounts) when only that is wanted.
    if !cfg.filesys.is_empty() || cfg.rom_scsi_device_disable {
        let slot = devices.len();
        zorro.add_board(crate::zorro::BoardSpec::copperline_services(slot))?;
        let mut board = crate::filesys::FilesysBoard::new(cfg.filesys.clone());
        if cfg.rom_scsi_device_disable {
            info!("romtags: the ROM's scsi.device will not be initialised");
            board.set_cull_rom_scsi_device(true);
        }
        info!(
            "filesys: services board on the Zorro chain (slot {slot}), {} mount(s)",
            cfg.filesys.len()
        );
        devices.push(crate::zorro_device::BoardDevice::Filesys(board));
    }
    // A2065 Ethernet board (in-tree LANCE NIC): networking is non-deterministic.
    if let Some(net_config) = &cfg.a2065_net {
        let slot = devices.len();
        zorro.add_board(crate::zorro::BoardSpec::a2065(slot))?;
        if matches!(
            net_config,
            crate::net::NetConfig::Nat | crate::net::NetConfig::Bridge { .. }
        ) {
            info!(
                "a2065: Ethernet board on the Zorro chain (slot {slot}), net backend \
                 {net_config:?} -- host networking is non-deterministic, replay/save-state \
                 reproducibility not guaranteed"
            );
        } else {
            info!(
                "a2065: Ethernet board on the Zorro chain (slot {slot}), net backend \
                 {net_config:?}"
            );
        }
        devices.push(crate::zorro_device::BoardDevice::A2065(
            crate::a2065::A2065::new(net_config.clone())?,
        ));
    }
    // Toccata sound board (`[toccata] enabled`): AD1848-based, its output
    // joins the mixer as the "toccata" audio source (see crate::toccata).
    if cfg.toccata {
        let slot = devices.len();
        zorro.add_board(crate::zorro::BoardSpec::toccata(slot))?;
        info!("toccata: AD1848 sound board on the Zorro chain (slot {slot})");
        devices.push(crate::zorro_device::BoardDevice::Toccata(Box::default()));
    }
    // MHI virtual MPEG audio decoder board (`[mhi] enabled`): decodes MP3
    // bitstream descriptors handed over via doorbell, its output joins the
    // mixer as the "mhi" audio source (see crate::mhi and
    // docs/internals/mhi.md).
    #[cfg(feature = "mhi")]
    if cfg.mhi {
        let slot = devices.len();
        zorro.add_board(crate::zorro::BoardSpec::mhi(slot))?;
        info!("mhi: MPEG audio decoder board on the Zorro chain (slot {slot})");
        devices.push(crate::zorro_device::BoardDevice::Mhi(Box::default()));
    }
    #[cfg(not(feature = "mhi"))]
    if cfg.mhi {
        log::warn!("[mhi] enabled = true needs a build with --features mhi; no board is fitted");
    }
    // RTG board (`[rtg] card`): the Z3660.card P96 driver drives RTG screens
    // through its register file and framebuffer; see crate::z3660.
    if cfg.rtg == crate::config::RtgCard::Z3660 {
        let slot = devices.len();
        zorro.add_board(crate::zorro::BoardSpec::z3660(slot))?;
        info!("z3660: RTG board on the Zorro chain (slot {slot})");
        devices.push(crate::zorro_device::BoardDevice::Z3660(
            crate::z3660::Z3660::new(),
        ));
    }
    // Village Tronic Picasso II/II+: one physical CL-GD542x device appears as
    // two consecutive Zorro II identities, linear VRAM first and the 64K VGA
    // register window second. Both resolve to the same device slot.
    if matches!(
        cfg.rtg,
        crate::config::RtgCard::Picasso2 | crate::config::RtgCard::Picasso2Plus
    ) {
        let plus = cfg.rtg == crate::config::RtgCard::Picasso2Plus;
        let slot = devices.len();
        let vram_spec = if plus {
            crate::zorro::BoardSpec::picasso2plus_vram(slot, cfg.rtg_vram_bytes)
        } else {
            crate::zorro::BoardSpec::picasso2_vram(slot, cfg.rtg_vram_bytes)
        };
        let regs_spec = if plus {
            crate::zorro::BoardSpec::picasso2plus_regs(slot)
        } else {
            crate::zorro::BoardSpec::picasso2_regs(slot)
        };
        zorro.add_board(vram_spec)?;
        zorro.add_board(regs_spec)?;
        info!(
            "{}: {} RTG board with {} MB VRAM on the Zorro chain (slot {slot})",
            if plus { "picasso2plus" } else { "picasso2" },
            if plus { "CL-GD5428" } else { "CL-GD5426" },
            cfg.rtg_vram_bytes / (1024 * 1024)
        );
        devices.push(crate::zorro_device::BoardDevice::Picasso2(Box::new(
            if plus {
                crate::picasso2::Picasso2::new_plus(cfg.rtg_vram_bytes)
            } else {
                crate::picasso2::Picasso2::new(cfg.rtg_vram_bytes)
            },
        )));
    }
    // Atéo Concepts Graffity [Zorro II]: the same CL-GD5428 core as Picasso
    // II+, wired as a chained VRAM + 128K register aperture pair.
    if cfg.rtg == crate::config::RtgCard::GraffityZ2 {
        let slot = devices.len();
        zorro.add_board(crate::zorro::BoardSpec::graffity_z2_vram(
            slot,
            cfg.rtg_vram_bytes,
        ))?;
        zorro.add_board(crate::zorro::BoardSpec::graffity_z2_regs(slot))?;
        info!(
            "graffityz2: CL-GD5428 RTG board with {} MB VRAM on the Zorro chain (slot {slot})",
            cfg.rtg_vram_bytes / (1024 * 1024)
        );
        devices.push(crate::zorro_device::BoardDevice::GraffityZ2(Box::new(
            crate::graffity::GraffityZ2::new(cfg.rtg_vram_bytes),
        )));
    }
    // Atéo Concepts Graffity [Zorro III]: one 16 MB autoconfig window over
    // the same CL-GD5428 core.
    if cfg.rtg == crate::config::RtgCard::GraffityZ3 {
        let slot = devices.len();
        zorro.add_board(crate::zorro::BoardSpec::graffity_z3(slot))?;
        info!(
            "graffityz3: CL-GD5428 RTG board with {} MB VRAM on the Zorro chain (slot {slot})",
            cfg.rtg_vram_bytes / (1024 * 1024)
        );
        devices.push(crate::zorro_device::BoardDevice::GraffityZ3(Box::new(
            crate::graffity::GraffityZ3::new(cfg.rtg_vram_bytes),
        )));
    }
    // The A1000 has no Kickstart ROM: cfg.rom_path is its 64 KiB bootstrap
    // ROM, and a 256 KiB WCS is allocated for it to load Kickstart into from
    // the Kickstart disk in DF0.
    let mut mem = if cfg.machine == Some(crate::config::MachineModel::A1000) {
        Memory::load_a1000(&cfg.rom_path, cfg.chip_ram_bytes, cfg.slow_ram_bytes, zorro)?
    } else if rom_optional && !cfg.rom_path.is_file() {
        // A save state restores the full ROM image, so a missing or sentinel
        // ROM path (the bundled-AROS placeholder, or a Kickstart the user no
        // longer keeps) is fine: build with a placeholder the state replaces.
        info!(
            "--load-state: ROM {} is unavailable; building with a placeholder \
             ROM that the save state will replace",
            cfg.rom_path.display()
        );
        Memory::placeholder(cfg.chip_ram_bytes, cfg.slow_ram_bytes, zorro)
    } else {
        Memory::load(&cfg.rom_path, cfg.chip_ram_bytes, cfg.slow_ram_bytes, zorro)?
    };
    if cfg.mb_ram_bytes > 0 {
        mem.fit_mb_ram(cfg.mb_ram_bytes);
        info!(
            "ramsey: {}K motherboard fast RAM at {:#010X}-{:#010X}",
            cfg.mb_ram_bytes / 1024,
            mem.mb_ram_base(),
            crate::memory::MB_RAM_TOP - 1
        );
    }
    if cfg.accel_ram_bytes > 0 {
        mem.fit_accel_ram(cfg.accel_ram_bytes);
        info!(
            "cpu slot: {}K accelerator fast RAM at {:#010X}-{:#010X}",
            cfg.accel_ram_bytes / 1024,
            crate::memory::ACCEL_RAM_BASE,
            crate::memory::ACCEL_RAM_BASE + cfg.accel_ram_bytes as u64 - 1
        );
    }
    if let Some(path) = &cfg.extended_rom_path {
        if rom_optional && !path.is_file() {
            // As with the main ROM above, the save state carries the extended
            // ROM image, so a missing file here is not fatal.
            info!(
                "--load-state: extended ROM {} is unavailable; the save state \
                 will supply it",
                path.display()
            );
        } else {
            let image = std::fs::read(path)
                .map_err(|e| anyhow!("reading extended ROM {}: {e}", path.display()))?;
            mem.attach_extended_rom(image)?;
            info!(
                "extended ROM: {} at {:#08X}",
                path.display(),
                mem.extended_rom_base
            );
        }
    }
    // System RAM is physically undefined after power is applied. Zero stays
    // the compatibility default; fixed and deterministic pseudo-random modes
    // help guest developers expose reads made before their own writes. Do this
    // before Bus::new so its first renderer snapshot sees the same bytes as
    // the CPU and DMA engines.
    mem.power_on_reset_with(cfg.ram_init);
    match cfg.ram_init {
        crate::memory::RamInit::Zero => {}
        crate::memory::RamInit::Pattern { word } => {
            info!("memory: fixed cold-start fill, word {word:#06X}");
        }
        crate::memory::RamInit::Random { seed } => {
            info!("memory: deterministic pseudo-random cold-start fill, seed {seed:#018X}");
        }
    }
    let mut cd_image = match &cfg.cd_image_path {
        Some(path) => {
            let image = crate::cdrom::CdImage::load(path)?;
            info!("cd image: {} ({})", path.display(), image.describe());
            Some(image)
        }
        None => None,
    };
    let mut floppy = FloppyController::from_config(&cfg.floppy)?;
    floppy.set_connected_drives(cfg.floppy_connected);
    #[cfg(feature = "fluxbridge")]
    attach_floppy_bridges(&mut floppy, cfg)?;
    let serial = build_serial_sink(cfg)?;
    let mut paula = Paula::new(serial, audio);
    paula
        .drive_sounds_mut()
        .set_enabled(cfg.audio.floppy_sounds);
    paula
        .drive_sounds_mut()
        .set_volume_percent(cfg.audio.floppy_sounds_volume);
    paula.set_mono_output(cfg.audio.channel_mode.is_mono());
    paula.set_stereo_separation(f32::from(cfg.audio.stereo_separation) / 100.0);
    paula.set_led_filter_mode(cfg.audio.filter);
    if cfg.audio.channel_mode.is_mono() && cfg.audio.stereo_separation != 100 {
        log::warn!("[audio] stereo_separation is ignored while channel_mode is mono");
    }
    let mut bus = Bus::new(mem, paula, floppy);
    bus.set_ram_init(cfg.ram_init);
    // The printer attaches here (its byte sink is `Send`). A sampler owns a
    // cpal capture stream that is `!Send` on some hosts, so the frontend builds
    // and attaches it on the main thread from a `SamplerRequest` instead.
    if cfg.parallel.device == crate::config::ParallelDevice::Printer {
        if let Some(path) = &cfg.parallel.printer_output {
            bus.attach_parallel_port(Box::new(crate::parallel::FileParallelPort::create(path)?));
            info!("parallel: capturing Centronics bytes to {}", path.display());
        }
    }
    bus.set_video_standard(cfg.video_standard);
    bus.set_chipset_revisions(cfg.agnus_revision, cfg.denise_revision);
    bus.set_rtc_present(cfg.rtc_present);
    bus.set_rtc_chip(cfg.rtc_chip);
    bus.rtc.set_seed(cfg.rtc_seed_unix, cfg.rtc_frozen);
    if let Some(path) = &cfg.battmem_path {
        info!("rtc: battery RAM (battmem) persisted to {}", path.display());
        bus.rtc.set_battmem_path(path.clone());
    }
    if let Some(id) = cfg.gate_array.gayle_id() {
        let mut gayle = crate::gayle::Gayle::new(id);
        if let Some(drive) = &cfg.ide.master {
            gayle.attach_drive(
                0,
                open_ide_target(
                    &drive.path,
                    0,
                    drive.volume_name.as_deref(),
                    drive.boot_pri,
                    drive.filesystem,
                )?,
            );
            info!("ide: master {}", drive.path.display());
        }
        if let Some(drive) = &cfg.ide.slave {
            gayle.attach_drive(
                1,
                open_ide_target(
                    &drive.path,
                    1,
                    drive.volume_name.as_deref(),
                    drive.boot_pri,
                    drive.filesystem,
                )?,
            );
            info!("ide: slave {}", drive.path.display());
        }
        // Real host disks last, so an image in the same slot has already been
        // refused by configuration validation rather than silently replaced.
        #[cfg(not(target_arch = "wasm32"))]
        attach_ide_host_disks(cfg, |slot, drive| gayle.attach_drive(slot, drive));
        bus.attach_gayle(gayle);
    }
    if cfg.ide_a4000 {
        let mut ide = crate::ide_a4000::IdeA4000::new();
        for (slot, drive) in [(0, &cfg.ide.master), (1, &cfg.ide.slave)] {
            let Some(drive) = drive else { continue };
            ide.attach_drive(
                slot,
                open_ide_target(
                    &drive.path,
                    slot,
                    drive.volume_name.as_deref(),
                    drive.boot_pri,
                    drive.filesystem,
                )?,
            );
            let which = if slot == 0 { "master" } else { "slave" };
            info!("ide: {which} {}", drive.path.display());
        }
        // The A4000's interface takes a real disk exactly as Gayle's does;
        // configuration accepts one for either, so both must wire it up.
        #[cfg(not(target_arch = "wasm32"))]
        attach_ide_host_disks(cfg, |slot, drive| ide.attach_drive(slot, drive));
        bus.attach_ide_a4000(ide);
        info!("ide: A4000 motherboard interface at $DD2020");
    }
    if let Some(range) = cfg.log_unmapped.clone() {
        info!(
            "debug: logging unmapped CPU accesses in {:#08X}-{:#08X}",
            range.start(),
            range.end()
        );
        bus.log_unmapped = Some(range);
    }
    if cfg.validate_chipset {
        info!("debug: chipset access validator armed");
        bus.set_chipset_validation(true);
    }
    if cfg.detect_smc {
        info!("debug: self-modifying-code detector armed");
        bus.set_smc_detection(true);
    }
    if cfg.sdmac {
        let mut sdmac = crate::sdmac::Sdmac::new();
        let mut drives = 0;
        if cfg.scsi.controller == crate::config::ScsiController::A3000 {
            for (unit, drive) in cfg.scsi.units.iter().enumerate() {
                let Some(drive) = drive else { continue };
                sdmac.attach_drive(unit, open_scsi_target(drive, unit)?);
                drives += 1;
            }
            #[cfg(not(target_arch = "wasm32"))]
            {
                drives +=
                    attach_scsi_host_disks(cfg, |unit, target| sdmac.attach_drive(unit, target));
            }
        }
        bus.attach_sdmac(sdmac);
        match drives {
            0 => info!("sdmac: Super DMAC + WD33C93 at $DD0000 (no drives)"),
            n => info!("sdmac: Super DMAC + WD33C93 at $DD0000, {n} drive(s)"),
        }
    }
    if let Some(revision) = cfg.mem_controller.ramsey_revision() {
        // Seed the control register with the DRAM geometry backing the fitted
        // motherboard RAM (stock geometry when none is fitted), so Kickstart's
        // sizing probe and the diagnostic tools read a description matching
        // the RAM that answers.
        let bank_bytes = revision.bank_bytes_for(cfg.mb_ram_bytes);
        bus.attach_ramsey(crate::ramsey::Ramsey::new(revision, bank_bytes));
    }
    // Gary and Ramsey share one address decode -- Gary owns byte lanes 0-2 of
    // the $DE0000 page and Ramsey lane 3 -- and tools probe Gary to find the
    // Ramsey: xSysInfo gives up on the memory controller if it cannot first
    // identify a Fat Gary. Fitting one without the other identifies as neither.
    if cfg.gate_array.is_fat_gary() {
        bus.attach_gary(crate::gary::Gary::new());
    }
    // The WinUAE-compatible uaelib trap at $F0FF60 (crate::uaelib). Fitted
    // even when a $F00000 extended ROM hides it: the decode order handles
    // both, and a later ROM reload may take the extended ROM away.
    if cfg.emulation.uaelib {
        let shadowed = !bus.mem.extended_rom.is_empty() && bus.mem.extended_rom_base == 0x00F0_0000;
        let mut uaelib = crate::uaelib::UaeLib::new();
        if cfg.emulation.uaelib_files {
            let root = cfg.run_program_dir.as_ref().and_then(|program_dir| {
                match std::fs::canonicalize(program_dir) {
                    Ok(path) => Some(path),
                    Err(e) => {
                        log::warn!(
                            "uaelib: cannot enable file commands below {}: {e}",
                            program_dir.display()
                        );
                        None
                    }
                }
            });
            match root {
                Some(root) => {
                    let display = root.display().to_string();
                    match uaelib.set_file_root(Some(root)) {
                        Ok(()) => info!("uaelib: debug_load/debug_save enabled below {display}"),
                        Err(e) => {
                            log::warn!("uaelib: cannot enable file commands below {display}: {e}")
                        }
                    }
                }
                None => log::warn!(
                    "[emulation] uaelib_files = true has no --run program directory; \
                     debug_load/debug_save remain disabled"
                ),
            }
        }
        bus.attach_uaelib(uaelib);
        if shadowed {
            info!("uaelib: trap at $F0FF60 fitted but hidden behind the $F00000 extended ROM");
        } else {
            info!(
                "uaelib: WinUAE-compatible trap at $F0FF60 (guest warp toggle, debug log, resources)"
            );
        }
    }
    if !devices.is_empty() {
        bus.attach_devices(devices);
    }
    bus.input.set_port_device(0, cfg.port_devices[0]);
    bus.input.set_port_device(1, cfg.port_devices[1]);
    info!(
        "input: port 1 = {}, port 2 = {}",
        cfg.port_devices[0].label(),
        cfg.port_devices[1].label()
    );
    if cfg.akiko {
        let mut akiko = crate::akiko::Akiko::new();
        if let Some(path) = &cfg.cd32_nvram_path {
            info!("akiko: NVRAM persisted to {}", path.display());
            akiko.set_nvram_path(path.clone());
        }
        if let Some(image) = cd_image.take() {
            akiko.insert_disc(image);
            info!("akiko: CD controller at $B80000, disc mounted");
        } else {
            info!("akiko: CD controller at $B80000, no disc");
        }
        bus.attach_akiko(akiko);
    }
    if cfg.cdtv_cd {
        let mut cdtv = crate::cdtv::CdtvController::new();
        if let Some(image) = cd_image.take() {
            if cfg.cd_insert_delay_secs > 0.0 {
                cdtv.insert_disc_after(image, cfg.cd_insert_delay_secs);
                info!(
                    "cdtv: DMAC/CD controller attached, disc inserts after {:.1}s",
                    cfg.cd_insert_delay_secs
                );
            } else {
                cdtv.insert_disc(image);
                info!("cdtv: DMAC/CD controller attached, disc mounted");
            }
        } else {
            info!("cdtv: DMAC/CD controller attached, no disc");
        }
        bus.attach_cdtv(cdtv);
    }
    if cd_image.is_some() {
        warn!("cd image configured but this machine has no CD controller; the disc is not mounted");
    }
    // The freezer cartridge (crate::cartridge): its bank at $A10000 and the
    // configuration block describing this machine, written from the boards
    // fitted so far, so it goes on after the IDE interfaces and the CD
    // controllers the block reports.
    if let Some(model) = cfg.cartridge.model {
        let path = cfg.cartridge.rom.as_ref().ok_or_else(|| {
            anyhow::anyhow!("[cartridge] model = \"{}\" needs an image", model.label())
        })?;
        let image = std::fs::read(path)
            .with_context(|| format!("reading cartridge image {}", path.display()))?;
        let cartridge = match model {
            crate::cartridge::CartridgeModel::Hrtmon => crate::cartridge::Cartridge::hrtmon(&image)
                .with_context(|| format!("cartridge image {}", path.display()))?,
        };
        let version = cartridge
            .version()
            .map_or_else(String::new, |(v, r)| format!(" {v}.{r:02}"));
        info!(
            "cartridge: {}{version} at {:#08X}-{:#08X} ({})",
            model.label(),
            cartridge.base(),
            cartridge.base() + cartridge.size() as u32 - 1,
            path.display()
        );
        bus.attach_cartridge(cartridge);
    }
    if let Some(machine) = cfg.machine {
        info!(
            "machine profile: {:?} (gate array {:?}, rtc {})",
            machine,
            cfg.gate_array,
            if cfg.rtc_present {
                cfg.rtc_chip.label()
            } else {
                "none"
            }
        );
    }
    let cpu_clocks_per_cck = crate::config::clocks_per_cck_for_mhz(cfg.cpu_clock_mhz);
    let mut emu = Emulator::new(
        bus,
        cfg.cpu,
        cfg.fpu,
        cfg.cpu_unimplemented,
        cfg.emulation.pacing_budget,
        cpu_clocks_per_cck,
        paced,
    )?;
    // The cache models stay active under JIT: on a real accelerator it is
    // exactly the caches that let chip-RAM-resident code run at CPU speed
    // instead of paying chip-bus arbitration per fetch (SysInfo's
    // Dhrystone on a fast-RAM-less Workbench regressed 3x without them).
    // There is no coherence hazard with the fastmem window: the window is
    // only offered while both cache models are absent (CpuBus::jit_fast_mem),
    // so cached configs simply run every access through the modelled bus.
    emu.set_cache_emulation(cfg.cpu_icache, cfg.cpu_dcache);
    emu.set_cpu_jit(cfg.cpu_jit);
    emu.set_machine_descriptor(cfg.descriptor());
    Ok(emu)
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
    fn reanchor_initializes_pre_run_restored_timeline() {
        let mut emu = emulator_with_audio(Box::new(crate::audio::NullSink));
        emu.paced = true;
        emu.bus_mut()
            .advance_devices(crate::chipset::paula::PAULA_CLOCK_HZ / 10);
        assert!(
            emu.stats.started_at.is_none(),
            "fixture should model a loaded state before the first paced frame"
        );

        let before = Instant::now();
        emu.reanchor_realtime_clock();
        let anchor = emu
            .stats
            .started_at
            .expect("reanchor should initialize the pacing clock");
        let target = super::realtime_device_time_target(anchor, emu.bus().emulated_seconds(), 0.0)
            .expect("finite target");

        assert!(
            target >= before,
            "target {target:?} should not remain before reanchor start {before:?}"
        );
        assert!(
            target <= Instant::now(),
            "target {target:?} should map the restored timeline to now"
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
                mb_ram: Vec::new(),
                accel_ram: Vec::new(),
                rom,
                overlay: false,
                zorro: crate::zorro::ZorroChain::default(),
                extended_rom: Vec::new(),
                extended_rom_base: 0,
                wcs: Vec::new(),
                wcs_write_protected: false,
            },
            crate::chipset::paula::Paula::new(Box::new(crate::serial::NullSerialSink), audio),
            crate::floppy::FloppyController::default(),
        );
        super::Emulator::new(
            bus,
            crate::config::CpuModel::M68000,
            false,
            Default::default(),
            crate::config::PacingBudget::Cycles,
            2,
            false,
        )
        .unwrap()
    }

    /// Build an emulator whose reset vector runs a tiny program in ROM:
    ///
    /// ```text
    /// F80010  BSR.S  $F80020   ; call the subroutine
    /// F80012  MOVEQ  #1,D0     ; return lands here (step-over stops before it)
    /// F80014  BRA.S  *         ; halt
    /// F80020  MOVEQ  #2,D1     ; subroutine body
    /// F80022  RTS
    /// ```
    ///
    /// SSP resets to $4000 (chip RAM), so BSR/RTS push and pop the return
    /// address through real memory. The reset vectors live in chip RAM (overlay
    /// is off, so the CPU reads them from address 0 at reset).
    /// An emulator running a self-modifying program out of chip RAM:
    ///
    /// ```text
    /// 030000  NOP                        ; the word that gets patched
    /// 030002  MOVE.W #$4E71,($30000).L
    /// 03000A  BRA.S  *
    /// ```
    ///
    /// The NOP retires before the store lands, so $30000 is known to be
    /// code by the time it is written over.
    fn emulator_with_self_modifying_program() -> super::Emulator {
        let mut emu = emulator_with_call_program();
        emu.bus_mut().mem.overlay = false;
        let program: [u16; 6] = [
            0x4E71, // NOP
            0x33FC, // MOVE.W #imm,(abs).L
            0x4E71, // immediate: NOP
            0x0003, // destination high
            0x0000, // destination low
            0x60FE, // BRA.S *
        ];
        let bytes: Vec<u8> = program.iter().flat_map(|w| w.to_be_bytes()).collect();
        emu.machine.debug_write_memory(0x30000, &bytes);
        emu.machine.debug_set_register(17, 0x30000);
        emu
    }

    #[test]
    fn the_smc_detector_reports_a_write_over_code_and_its_prefetch_distance() {
        let mut emu = emulator_with_self_modifying_program();
        emu.bus_mut().set_smc_detection(true);
        for _ in 0..4 {
            emu.debug_step_instructions(1).unwrap();
        }
        let (reports, dropped) = emu.bus().smc_reports();
        assert_eq!(dropped, 0);
        assert_eq!(reports.len(), 1, "{reports:?}");
        assert_eq!(reports[0].addr, 0x30000);
        assert_eq!(reports[0].writer_pc, 0x30002);
        let line = crate::smc::SmcTracker::describe(&reports[0]);
        assert!(line.contains("2 bytes behind"), "{line}");
    }

    #[test]
    fn an_unarmed_machine_records_no_self_modifying_writes() {
        let mut emu = emulator_with_self_modifying_program();
        for _ in 0..4 {
            emu.debug_step_instructions(1).unwrap();
        }
        assert!(emu.bus().smc_reports().0.is_empty());
        // Arming after the fact starts from a blank execution map, so it
        // reports only what it actually watched.
        emu.bus_mut().set_smc_detection(true);
        assert!(emu.bus().smc_reports().0.is_empty());
    }

    #[test]
    fn armed_diagnostics_survive_a_state_restore() {
        use crate::bus::FaultInjection;
        let mut emu = emulator_with_call_program();
        emu.bus_mut().set_chipset_validation(true);
        emu.bus_mut().set_smc_detection(true);
        emu.bus_mut()
            .set_heat_map(Some((0, crate::heatmap::DEFAULT_SPAN)));
        emu.bus_mut().inject_bus_fault(FaultInjection {
            start: 0x40000,
            end: 0x40001,
            on_read: true,
            on_write: true,
            remaining: None,
            hits: 0,
        });
        // Collect a finding, then round-trip the machine through a state
        // save and load, which is the path a reverse step takes.
        emu.bus_mut().custom_write(0xDFF104, 2, 0x8020);
        assert_eq!(emu.bus().chipset_findings().0.len(), 1);
        let path = std::env::temp_dir().join(format!(
            "copperline-diag-restore-{}.clstate",
            std::process::id()
        ));
        emu.save_state(&path).unwrap();
        emu.load_state(&path).unwrap();
        std::fs::remove_file(&path).ok();

        // The session asked for these; a restore must not silently
        // disarm them underneath it, and the report is still the
        // operator's in-progress experiment.
        assert!(emu.bus().chipset_validation_armed());
        assert!(emu.bus().smc_detection_armed());
        assert!(emu.bus().heat_map().is_some());
        assert_eq!(emu.bus().injected_bus_faults().len(), 1);
        assert_eq!(emu.bus().chipset_findings().0.len(), 1);
    }

    #[test]
    fn an_injected_bus_fault_takes_the_guest_into_its_own_handler() {
        use crate::bus::FaultInjection;
        let mut emu = emulator_with_self_modifying_program();
        // Point the bus-error vector (2) somewhere recognisable.
        emu.machine
            .debug_write_memory(0x08, &0x0003_1000u32.to_be_bytes());
        emu.machine
            .debug_write_memory(0x31000, &0x4E71u16.to_be_bytes());
        // One shot on the program's own store target.
        emu.bus_mut().inject_bus_fault(FaultInjection {
            start: 0x30000,
            end: 0x30001,
            on_read: false,
            on_write: true,
            remaining: Some(1),
            hits: 0,
        });
        for _ in 0..6 {
            emu.debug_step_instructions(1).unwrap();
        }
        assert_eq!(
            emu.bus().injected_bus_faults()[0].hits,
            1,
            "the write should have taken the fault"
        );
        // The store never reached memory, so the NOP it aimed at is
        // untouched, and the CPU is running the handler.
        assert_eq!(emu.bus().peek_word_any(0x30000), 0x4E71);
        assert!(
            (0x31000..0x31010).contains(&emu.machine.pc()),
            "expected the bus-error handler, pc = {:06X}",
            emu.machine.pc()
        );
    }

    #[test]
    fn a_counted_bus_fault_stops_firing_once_it_is_spent() {
        use crate::bus::FaultInjection;
        let mut emu = emulator_with_call_program();
        emu.bus_mut().mem.overlay = false;
        emu.bus_mut().inject_bus_fault(FaultInjection {
            start: 0x40000,
            end: 0x40001,
            on_read: true,
            on_write: true,
            remaining: Some(2),
            hits: 0,
        });
        let fired: Vec<bool> = (0..4)
            .map(|_| emu.bus_mut().take_injected_fault(0x40000, 2, false))
            .collect();
        assert_eq!(fired, [true, true, false, false]);
        let fault = emu.bus().injected_bus_faults()[0];
        assert_eq!(fault.hits, 2);
        assert_eq!(fault.remaining, Some(0));
        // An address outside the window is never faulted.
        assert!(!emu.bus_mut().take_injected_fault(0x50000, 2, false));
    }

    fn emulator_with_call_program() -> super::Emulator {
        let mut rom = vec![0u8; crate::memory::ROM_SIZE];
        let put = |mem: &mut [u8], off: usize, word: u16| {
            mem[off..off + 2].copy_from_slice(&word.to_be_bytes());
        };
        put(&mut rom, 0x10, 0x610E); // BSR.S $F80020
        put(&mut rom, 0x12, 0x7001); // MOVEQ #1,D0
        put(&mut rom, 0x14, 0x60FE); // BRA.S *
        put(&mut rom, 0x20, 0x7202); // MOVEQ #2,D1
        put(&mut rom, 0x22, 0x4E75); // RTS

        let mut chip_ram = vec![0u8; 512 * 1024];
        chip_ram[0..4].copy_from_slice(&0x0000_4000u32.to_be_bytes()); // reset SSP
        chip_ram[4..8].copy_from_slice(&0x00F8_0010u32.to_be_bytes()); // reset PC

        let bus = crate::bus::Bus::new(
            crate::memory::Memory {
                chip_ram,
                slow_ram: Vec::new(),
                mb_ram: Vec::new(),
                accel_ram: Vec::new(),
                rom,
                overlay: false,
                zorro: crate::zorro::ZorroChain::default(),
                extended_rom: Vec::new(),
                extended_rom_base: 0,
                wcs: Vec::new(),
                wcs_write_protected: false,
            },
            crate::chipset::paula::Paula::new(
                Box::new(crate::serial::NullSerialSink),
                Box::new(crate::audio::NullSink),
            ),
            crate::floppy::FloppyController::default(),
        );
        super::Emulator::new(
            bus,
            crate::config::CpuModel::M68000,
            false,
            Default::default(),
            crate::config::PacingBudget::Cycles,
            2,
            false,
        )
        .unwrap()
    }

    /// Like [`emulator_with_call_program`], but the program executes a
    /// TRAP #0 whose handler (vector 32, set up in chip RAM) parks at
    /// $F80030:
    ///
    /// ```text
    /// F80010  NOP
    /// F80012  TRAP   #0
    /// F80014  BRA.S  *
    /// F80030  BRA.S  *          ; trap handler
    /// ```
    fn emulator_with_trap_program() -> super::Emulator {
        let mut rom = vec![0u8; crate::memory::ROM_SIZE];
        let put = |mem: &mut [u8], off: usize, word: u16| {
            mem[off..off + 2].copy_from_slice(&word.to_be_bytes());
        };
        put(&mut rom, 0x10, 0x4E71); // NOP
        put(&mut rom, 0x12, 0x4E40); // TRAP #0
        put(&mut rom, 0x14, 0x60FE); // BRA.S *
        put(&mut rom, 0x30, 0x60FE); // handler: BRA.S *

        let mut chip_ram = vec![0u8; 512 * 1024];
        chip_ram[0..4].copy_from_slice(&0x0000_4000u32.to_be_bytes()); // reset SSP
        chip_ram[4..8].copy_from_slice(&0x00F8_0010u32.to_be_bytes()); // reset PC
        chip_ram[32 * 4..32 * 4 + 4].copy_from_slice(&0x00F8_0030u32.to_be_bytes()); // TRAP #0

        let bus = crate::bus::Bus::new(
            crate::memory::Memory {
                chip_ram,
                slow_ram: Vec::new(),
                mb_ram: Vec::new(),
                accel_ram: Vec::new(),
                rom,
                overlay: false,
                zorro: crate::zorro::ZorroChain::default(),
                extended_rom: Vec::new(),
                extended_rom_base: 0,
                wcs: Vec::new(),
                wcs_write_protected: false,
            },
            crate::chipset::paula::Paula::new(
                Box::new(crate::serial::NullSerialSink),
                Box::new(crate::audio::NullSink),
            ),
            crate::floppy::FloppyController::default(),
        );
        super::Emulator::new(
            bus,
            crate::config::CpuModel::M68000,
            false,
            Default::default(),
            crate::config::PacingBudget::Cycles,
            2,
            false,
        )
        .unwrap()
    }

    #[test]
    fn step_over_runs_the_callee_and_stops_after_the_call() {
        let mut emu = emulator_with_call_program();
        assert_eq!(emu.machine.pc(), 0x00F8_0010);
        emu.debug_step_over(10_000).unwrap();
        // The subroutine ran to completion (D1=2) and we stopped at the
        // instruction after the BSR, before it executed (D0 still 0).
        assert_eq!(emu.machine.pc(), 0x00F8_0012);
        assert_eq!(emu.machine.d(1), 2);
        assert_eq!(emu.machine.d(0), 0);
    }

    #[test]
    fn step_over_a_non_call_is_a_plain_single_step() {
        let mut emu = emulator_with_call_program();
        emu.debug_step_over(10_000).unwrap(); // over the BSR -> at $F80012
        emu.debug_step_over(10_000).unwrap(); // MOVEQ is not a call: single step
        assert_eq!(emu.machine.pc(), 0x00F8_0014);
        assert_eq!(emu.machine.d(0), 1);
    }

    #[test]
    fn conditional_breakpoint_fires_during_execution() {
        use crate::debugger::{BreakCond, CondOp, CondOperand};
        let mut emu = emulator_with_call_program();
        // Break at the subroutine entry only when D1 == 0 (true on first
        // entry; the callee sets D1=2 afterwards).
        emu.machine.ui_set_breakpoint(
            0x00F8_0020,
            Some(BreakCond {
                lhs: CondOperand::Data(1),
                op: CondOp::Eq,
                rhs: CondOperand::Imm(0),
            }),
            0,
        );
        let mut stopped = false;
        for _ in 0..32 {
            emu.debug_step_instructions(1).unwrap();
            if emu.machine.ui_debug_stop_pending() {
                stopped = true;
                break;
            }
        }
        assert!(stopped, "conditional breakpoint did not fire");
        assert_eq!(emu.machine.pc(), 0x00F8_0020);
        assert!(emu.machine.take_ui_debug_stop().is_some());
    }

    #[test]
    fn watchpoint_catches_a_bitplane_dma_fetch_of_the_watched_word() {
        use crate::debugger::{DebugStop, WatchSource};
        let mut emu = emulator_with_call_program();
        emu.bus_mut().mem.overlay = false;
        // Watch a chip-RAM word and point bitplane 1 at it, then let a
        // frame's display DMA run. A fetch changes nothing, so only the
        // per-channel read attribution can see this at all.
        assert!(emu.machine.ui_toggle_watch_access(
            0x60000,
            None,
            None,
            crate::debugger::WatchAccess::Read,
        ));
        {
            let bus = emu.bus_mut();
            bus.custom_write(0x0E0, 4, 0x0006_0000); // BPL1PT = $60000
            bus.custom_write(0x100, 2, 0x1200); // BPLCON0: 1 plane, lores
            bus.custom_write(0x092, 2, 0x0038); // DDFSTRT
            bus.custom_write(0x094, 2, 0x00D0); // DDFSTOP
            bus.custom_write(0x08E, 2, 0x2C81); // DIWSTRT
            bus.custom_write(0x090, 2, 0xF4C1); // DIWSTOP
            bus.custom_write(0x096, 2, 0x8300); // DMACON SET DMAEN|BPLEN
        }
        let mut stop = None;
        // Display DMA only runs inside the vertical window, so give it
        // whole frames rather than an instruction budget.
        for _ in 0..3 {
            emu.step_frame().unwrap();
            if let Some(s) = emu.machine.take_ui_debug_stop() {
                stop = Some(s);
                break;
            }
        }
        match stop {
            Some(DebugStop::Watch {
                source, old, new, ..
            }) => {
                assert_eq!(source, WatchSource::Bitplane(0));
                assert_eq!(old, new, "a fetch must not be reported as a change");
            }
            other => panic!("expected a bitplane-attributed watch stop, got {other:?}"),
        }
    }

    #[test]
    fn a_pc_qualified_watch_ignores_writes_from_other_instructions() {
        use crate::debugger::WatchSource;
        let mut emu = emulator_with_call_program();
        emu.bus_mut().mem.overlay = false;
        // Qualify on a PC that never executes: the blitter write below
        // still lands, but no stop belongs to that instruction.
        assert!(emu.machine.ui_toggle_watch_qualified(
            0x60000,
            Some(WatchSource::Cpu),
            Some(0x00F8_0F00)
        ));
        {
            let bus = emu.bus_mut();
            bus.custom_write(0x096, 2, 0x8240);
            bus.custom_write(0x040, 2, 0x01F0);
            bus.custom_write(0x042, 2, 0x0000);
            bus.custom_write(0x044, 2, 0xFFFF);
            bus.custom_write(0x046, 2, 0xFFFF);
            bus.custom_write(0x074, 2, 0xBEEF);
            bus.custom_write(0x054, 4, 0x0006_0000);
            bus.custom_write(0x058, 2, 0x0041);
        }
        for _ in 0..64 {
            emu.debug_step_instructions(1).unwrap();
            assert!(
                emu.machine.take_ui_debug_stop().is_none(),
                "a PC-qualified watch must not fire for another writer"
            );
        }
        assert_eq!(
            emu.bus().peek_word_any(0x60000),
            0xBEEF,
            "the blit still ran"
        );
    }

    #[test]
    fn watchpoint_attributes_blitter_writes_and_honours_filters() {
        use crate::debugger::{DebugStop, WatchSource};
        let mut emu = emulator_with_call_program();
        emu.bus_mut().mem.overlay = false;
        assert!(emu.machine.ui_toggle_watch(0x60000));

        // A 1x1 D-only blit (LF = A, ADAT latched) into the watched word,
        // with blitter DMA enabled.
        {
            let bus = emu.bus_mut();
            bus.custom_write(0x096, 2, 0x8240); // DMACON SET DMAEN|BLTEN
            bus.custom_write(0x040, 2, 0x01F0); // BLTCON0: USED, LF=$F0 (D=A)
            bus.custom_write(0x042, 2, 0x0000); // BLTCON1
            bus.custom_write(0x044, 2, 0xFFFF); // BLTAFWM
            bus.custom_write(0x046, 2, 0xFFFF); // BLTALWM
            bus.custom_write(0x074, 2, 0xBEEF); // BLTADAT
            bus.custom_write(0x054, 4, 0x0006_0000); // BLTDPT
            bus.custom_write(0x058, 2, 0x0041); // BLTSIZE: 1 row x 1 word
        }
        let mut stop = None;
        for _ in 0..64 {
            emu.debug_step_instructions(1).unwrap();
            if let Some(s) = emu.machine.take_ui_debug_stop() {
                stop = Some(s);
                break;
            }
        }
        match stop {
            Some(DebugStop::Watch { source, new, .. }) => {
                assert_eq!(source, WatchSource::Blitter);
                assert_eq!(new, 0xBEEF);
            }
            other => panic!("expected a blitter-attributed watch stop, got {other:?}"),
        }

        // A CPU-filtered watch swallows the same blitter write.
        let mut emu = emulator_with_call_program();
        emu.bus_mut().mem.overlay = false;
        assert!(emu
            .machine
            .ui_toggle_watch_filtered(0x60000, Some(WatchSource::Cpu)));
        {
            let bus = emu.bus_mut();
            bus.custom_write(0x096, 2, 0x8240);
            bus.custom_write(0x040, 2, 0x01F0);
            bus.custom_write(0x042, 2, 0x0000);
            bus.custom_write(0x044, 2, 0xFFFF);
            bus.custom_write(0x046, 2, 0xFFFF);
            bus.custom_write(0x074, 2, 0xBEEF);
            bus.custom_write(0x054, 4, 0x0006_0000);
            bus.custom_write(0x058, 2, 0x0041);
        }
        for _ in 0..64 {
            emu.debug_step_instructions(1).unwrap();
            assert!(
                emu.machine.take_ui_debug_stop().is_none(),
                "cpu-filtered watch must swallow a blitter write"
            );
        }
        assert_eq!(emu.bus().peek_word_any(0x60000), 0xBEEF);
    }

    #[test]
    fn reverse_continue_lands_on_a_watchpoint_hit() {
        use crate::timetravel::ReverseOutcome;
        // NOP, NOP, MOVE.W #$1234,$60000.L, NOPs, BRA.S *.
        let mut rom = vec![0u8; crate::memory::ROM_SIZE];
        let put = |mem: &mut [u8], off: usize, word: u16| {
            mem[off..off + 2].copy_from_slice(&word.to_be_bytes());
        };
        put(&mut rom, 0x10, 0x4E71);
        put(&mut rom, 0x12, 0x4E71);
        put(&mut rom, 0x14, 0x33FC);
        put(&mut rom, 0x16, 0x1234);
        put(&mut rom, 0x18, 0x0006);
        put(&mut rom, 0x1A, 0x0000);
        put(&mut rom, 0x1C, 0x4E71);
        put(&mut rom, 0x1E, 0x4E71);
        put(&mut rom, 0x20, 0x60FE);
        let mut chip_ram = vec![0u8; 512 * 1024];
        chip_ram[0..4].copy_from_slice(&0x0000_4000u32.to_be_bytes());
        chip_ram[4..8].copy_from_slice(&0x00F8_0010u32.to_be_bytes());
        let bus = crate::bus::Bus::new(
            crate::memory::Memory {
                chip_ram,
                slow_ram: Vec::new(),
                mb_ram: Vec::new(),
                accel_ram: Vec::new(),
                rom,
                overlay: false,
                zorro: crate::zorro::ZorroChain::default(),
                extended_rom: Vec::new(),
                extended_rom_base: 0,
                wcs: Vec::new(),
                wcs_write_protected: false,
            },
            crate::chipset::paula::Paula::new(
                Box::new(crate::serial::NullSerialSink),
                Box::new(crate::audio::NullSink),
            ),
            crate::floppy::FloppyController::default(),
        );
        let mut emu = super::Emulator::new(
            bus,
            crate::config::CpuModel::M68000,
            false,
            Default::default(),
            crate::config::PacingBudget::Cycles,
            2,
            false,
        )
        .unwrap();
        emu.enable_time_travel(64, 1);
        emu.debug_ensure_time_travel_anchor().unwrap();
        assert!(emu.machine.ui_toggle_watch(0x60000));

        // Run forward well past the write, draining the forward stop.
        for _ in 0..12 {
            emu.debug_step_instructions(1).unwrap();
        }
        assert_eq!(emu.bus().peek_word_any(0x60000), 0x1234);
        let _ = emu.machine.take_ui_debug_stop();
        let here = emu.retired_instructions();

        // Reverse-continue lands just after the watched write, with the
        // watch hit as the reason.
        match emu.tt_reverse_continue().unwrap() {
            ReverseOutcome::Found((pos, reason)) => {
                assert!(pos < here, "landed at {pos}, started at {here}");
                assert!(
                    reason.contains("Watch $060000") && reason.contains("0000->1234"),
                    "{reason}"
                );
                // At the landing boundary the write has just retired.
                assert_eq!(emu.bus().peek_word_any(0x60000), 0x1234);
                assert_eq!(emu.machine.pc() & 0x00FF_FFFF, 0x00F8_001C);
            }
            other => panic!("expected a watch landing, got {other:?}"),
        }
        // Beam traps survive the timeline jump (adopt_ui_debug_state).
        emu.bus_mut().ui_arm_beam_trap_once(100, None);
        let _ = emu.tt_reverse_step(1).unwrap();
        assert_eq!(emu.bus().ui_beam_traps().len(), 1);
    }

    #[test]
    fn run_to_beam_stops_at_the_position_with_a_beam_stop() {
        use crate::debugger::DebugStop;
        let mut emu = emulator_with_call_program();
        let start_vpos = emu.bus().agnus.vpos;
        let target = (start_vpos + 3).min(u32::from(u16::MAX)) as u16;
        let reached = emu.debug_run_to_beam(target, Some(40), 100_000).unwrap();
        assert!(reached, "beam target three lines ahead must be reachable");
        match emu.machine.take_ui_debug_stop() {
            Some(DebugStop::Beam { vpos, hpos }) => {
                assert_eq!(vpos, target);
                assert_eq!(hpos, 40);
            }
            other => panic!("expected a beam stop, got {other:?}"),
        }
        // The machine stopped at (or just past) the trap position, and the
        // one-shot trap is gone.
        let bus = emu.bus();
        assert!(
            (bus.agnus.vpos, bus.agnus.hpos) >= (u32::from(target), 40),
            "beam should be at or past the target, is ({}, {})",
            bus.agnus.vpos,
            bus.agnus.hpos
        );
        assert!(bus.ui_beam_traps().is_empty());
    }

    #[test]
    fn exception_catchpoint_stops_on_trap_entry() {
        use crate::debugger::DebugStop;
        let mut emu = emulator_with_trap_program();
        assert!(emu.machine.ui_toggle_catch(32)); // TRAP #0
        let mut stopped = false;
        for _ in 0..64 {
            emu.debug_step_instructions(1).unwrap();
            if emu.machine.ui_debug_stop_pending() {
                stopped = true;
                break;
            }
        }
        assert!(stopped, "TRAP #0 catchpoint did not fire");
        match emu.machine.take_ui_debug_stop() {
            Some(DebugStop::Exception { vector, .. }) => assert_eq!(vector, 32),
            other => panic!("expected an exception stop, got {other:?}"),
        }
        // The machine sits in the trap handler.
        assert_eq!(emu.machine.pc() & 0x00FF_FFFF, 0x00F8_0030);
    }

    #[test]
    fn exception_catchpoint_stops_on_vblank_interrupt() {
        use crate::debugger::DebugStop;
        let mut emu = emulator_with_call_program();
        {
            let bus = emu.bus_mut();
            // Vector 27 (autovector level 3) -> a handler parked in ROM.
            bus.mem.chip_ram[27 * 4..27 * 4 + 4].copy_from_slice(&0x00F8_0014u32.to_be_bytes());
            // Enable master + VERTB interrupts.
            assert!(!bus.custom_write(0x09A, 2, 0xC020));
        }
        // The reset SR masks all interrupt levels (IPL 7); open the mask so
        // the level-3 vertical blank can be recognized.
        assert!(emu.machine.debug_set_register(16, 0x2000));
        assert!(emu.machine.ui_toggle_catch(27));
        let mut stopped = false;
        for _ in 0..200_000 {
            emu.debug_step_instructions(1).unwrap();
            if emu.machine.ui_debug_stop_pending() {
                stopped = true;
                break;
            }
        }
        assert!(
            stopped,
            "VERTB catchpoint did not fire within budget: frames={} pc={:08X} sr={:04X}",
            emu.bus().emulated_frames(),
            emu.machine.pc(),
            emu.machine.sr(),
        );
        match emu.machine.take_ui_debug_stop() {
            Some(DebugStop::Exception { vector, pc }) => {
                assert_eq!(vector, 27);
                // Stopped at the handler's entry, before it executes.
                assert_eq!(pc, 0x00F8_0014);
            }
            other => panic!("expected an exception stop, got {other:?}"),
        }
    }

    #[test]
    fn copper_step_advances_one_instruction_at_a_time() {
        let mut emu = emulator_with_call_program();
        // WAIT v2 / MOVE / WAIT v4 / MOVE / end: each step has bounded
        // work, and the WAITs park the Copper between steps.
        {
            let bus = emu.bus_mut();
            let cop1 = 0x0300usize;
            let words: [u16; 10] = [
                0x0201, 0xFFFE, // WAIT v>=2
                0x0180, 0x0111, // MOVE COLOR00
                0x0401, 0xFFFE, // WAIT v>=4
                0x0182, 0x0222, // MOVE COLOR01
                0xFFFF, 0xFFFE, // end of list
            ];
            for (idx, word) in words.iter().enumerate() {
                bus.mem.chip_ram[cop1 + idx * 2..cop1 + idx * 2 + 2]
                    .copy_from_slice(&word.to_be_bytes());
            }
            bus.agnus.dmacon |= 0x0280; // DMAEN | COPEN
            bus.copper.jump(cop1 as u32);
        }
        let before = emu.bus().copper_instructions_retired();
        assert!(emu.debug_step_copper(50_000).unwrap());
        let after_one = emu.bus().copper_instructions_retired();
        assert!(
            after_one > before,
            "copper step must retire at least one instruction"
        );
        // Stepping again advances further (the machine pauses at CPU
        // instruction boundaries, so each step retires one or more).
        assert!(emu.debug_step_copper(50_000).unwrap());
        assert!(emu.bus().copper_instructions_retired() > after_one);
    }

    #[test]
    fn run_to_beam_budget_exhaustion_disarms_the_one_shot() {
        let mut emu = emulator_with_call_program();
        // A tiny budget cannot reach a position a full frame away.
        let reached = emu.debug_run_to_beam(200, None, 4).unwrap();
        assert!(!reached);
        assert!(
            emu.bus().ui_beam_traps().is_empty(),
            "an unreached run-to trap must not stay armed"
        );
    }

    #[test]
    fn step_out_returns_to_the_caller() {
        let mut emu = emulator_with_call_program();
        emu.debug_step_instructions(1).unwrap(); // execute the BSR -> inside callee
        assert_eq!(emu.machine.pc(), 0x00F8_0020);
        emu.debug_step_out(10_000).unwrap();
        // Returned to the instruction after the call; the callee body ran.
        assert_eq!(emu.machine.pc(), 0x00F8_0012);
        assert_eq!(emu.machine.d(1), 2);
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

    #[derive(Default)]
    struct RunaheadProbe {
        pushed: std::cell::Cell<u64>,
    }

    struct RunaheadProbeSink(std::rc::Rc<RunaheadProbe>);

    impl crate::audio::AudioSink for RunaheadProbeSink {
        fn push(&mut self, _left: f32, _right: f32) {
            self.0.pushed.set(self.0.pushed.get() + 1);
        }
        fn flush(&mut self) {}
    }

    #[test]
    fn runahead_restore_rewinds_machine_state_and_keeps_positions_monotonic() {
        let mut emu = emulator_with_audio(Box::new(RunaheadProbeSink(std::rc::Rc::new(
            RunaheadProbe::default(),
        ))));
        emu.step_frame().unwrap();
        let retired_at_anchor = emu.retired_instructions;
        let blob = emu.runahead_snapshot().unwrap();

        // Advance past the anchor and leave a marker in chip RAM.
        emu.step_frame().unwrap();
        assert!(emu.retired_instructions > retired_at_anchor);
        emu.bus_mut().mem.chip_ram[0x2000] = 0xAB;

        emu.runahead_restore(&blob).unwrap();
        assert_eq!(
            emu.bus().mem.chip_ram[0x2000],
            0,
            "writes after the anchor are rolled back by the restore"
        );
        assert!(
            emu.retired_instructions >= retired_at_anchor,
            "the position coordinate stays monotonic across an anchor restore"
        );
    }

    #[test]
    fn speculative_audio_is_withheld_from_the_sink() {
        let probe = std::rc::Rc::new(RunaheadProbe::default());
        let mut emu = emulator_with_audio(Box::new(RunaheadProbeSink(probe.clone())));
        emu.set_runahead_speculative(true);
        emu.step_frame().unwrap();
        assert_eq!(probe.pushed.get(), 0);

        emu.set_runahead_speculative(false);
        emu.step_frame().unwrap();
        assert!(probe.pushed.get() > 0, "committed output flows again");
    }

    #[test]
    fn speculative_frames_do_not_inflate_committed_frame_statistics() {
        let mut emu = emulator_with_audio(Box::new(crate::audio::NullSink));
        emu.step_frame().unwrap();
        let committed = emu.stats.frames;

        emu.set_runahead_speculative(true);
        emu.step_frame().unwrap();
        assert_eq!(emu.stats.frames, committed);

        emu.set_runahead_speculative(false);
        emu.step_frame().unwrap();
        assert_eq!(emu.stats.frames, committed + 1);
    }

    #[test]
    fn audio_debug_taps_survive_a_runahead_restore() {
        let mut emu = emulator_with_audio(Box::new(crate::audio::NullSink));
        emu.step_frame().unwrap();
        let blob = emu.runahead_snapshot().unwrap();
        emu.bus_mut().paula.toggle_channel_muted(2);
        emu.runahead_restore(&blob).unwrap();
        assert!(emu.bus().paula.channel_muted(2));
    }
}
