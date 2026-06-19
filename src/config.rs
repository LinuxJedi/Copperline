// SPDX-License-Identifier: GPL-3.0-or-later

//! Loadable configuration. The file format is TOML; see
//! `copperline.example.toml` (or the README) for the full schema.

use crate::chipset::agnus::{AgnusRevision, VideoStandard};
use crate::chipset::denise::DeniseRevision;
use crate::zorro::{zorro_ii_size_code, zorro_iii_size_bits, BoardSpec, ZorroChain, ZorroVersion};
use anyhow::{anyhow, bail, Context, Result};
use serde::Deserialize;
use std::io::Read;
use std::path::{Path, PathBuf};

/// Sentinel `rom_path` meaning "the user named no ROM": boot the bundled
/// AROS open-source Kickstart replacement if it can be found, otherwise fail
/// with a message telling the user to supply a Kickstart. A real path (from
/// `rom = "..."` or the CLI argument) always replaces it.
pub const BUNDLED_AROS_ROM: &str = "<bundled-aros>";

#[derive(Debug, Clone)]
pub struct Config {
    pub rom_path: PathBuf,
    pub cpu: CpuModel,
    pub fpu: bool,
    /// CPU clock in MHz. Defaults to the model's stock speed
    /// ([`CpuModel::default_clock_mhz`]); overridable via `[cpu] clock_mhz`.
    pub cpu_clock_mhz: f64,
    /// Model the 68020/030 on-chip instruction cache (CACR-controlled).
    /// Defaults on for the parts that have one (68EC020/68020/68030), as on
    /// real silicon; `[cpu] icache = false` opts a 020/030 back out.
    pub cpu_icache: bool,
    /// Model the 68030 on-chip data cache (CACR-controlled). Defaults on for
    /// the 68030. Only caches expansion RAM and ROM (chip/slow RAM get
    /// cache inhibit, as on real Amigas, because DMA writes them).
    pub cpu_dcache: bool,
    pub emulation: Emulation,
    pub chip_ram_bytes: usize,
    pub fast_ram_bytes: usize,
    pub slow_ram_bytes: usize,
    /// Zorro III autoconfig RAM (`[memory] z3`). Needs a CPU with a 32-bit
    /// address bus (68020/030/040; not the 24-bit 68000/68EC020).
    pub z3_ram_bytes: usize,
    /// Extra Zorro boards loaded from `[[zorro]]` metadata files, in
    /// autoconfig chain order after the built-in RAM boards.
    pub zorro_boards: Vec<BoardSpec>,
    pub chipset: Chipset,
    /// Concrete chip revisions derived from the `[chipset] revision` preset,
    /// installed chip RAM, and the optional `agnus`/`denise` overrides.
    pub agnus_revision: AgnusRevision,
    pub denise_revision: DeniseRevision,
    /// Selected machine profile, if a `[machine]` section was given.
    pub machine: Option<MachineModel>,
    pub gate_array: GateArray,
    /// Akiko gate array fitted (CD32 profile): ID + C2P port at $B80000.
    pub akiko: bool,
    /// CDTV DMAC/CD controller fitted (CDTV profile): a Zorro II
    /// autoconfig board carrying the 6525 TPI and the Matshita drive.
    pub cdtv_cd: bool,
    /// A CD32 joypad is plugged into port 2 (CD32 profile): enables the
    /// pad's serial button protocol for lowlevel.library.
    pub cd32_pad: bool,
    /// Extended ROM image (`extended_rom = "path"`): 512 KiB maps at
    /// $E00000 (CD32), 256 KiB at $F00000 (CDTV).
    pub extended_rom_path: Option<PathBuf>,
    /// CD image (`[cd] image = "disc.cue"`), mounted on the machine's CD
    /// controller (CD32 Akiko or CDTV DMAC).
    pub cd_image_path: Option<PathBuf>,
    /// Emulated seconds after power-on at which the CD is inserted
    /// (0 = present at boot). Some CDTV discs need a post-boot insert.
    pub cd_insert_delay_secs: f64,
    /// CD32 NVRAM backing file (None = session-only EEPROM).
    pub cd32_nvram_path: Option<PathBuf>,
    /// Whether the MSM6242 RTC at $DC0000 is fitted. Defaults to true
    /// (legacy behaviour); the base A600 shipped without one.
    pub rtc_present: bool,
    pub video_standard: VideoStandard,
    pub audio: AudioConfig,
    /// Gayle IDE drive images (raw flat HDF, RDB inside), opened
    /// read/write. Only valid on machines with a Gayle gate array.
    pub ide: IdeConfig,
    /// A2091 SCSI controller (`[scsi]`): boot ROM image plus up to seven
    /// drive images on SCSI IDs 0-6. Works on any machine model (the board
    /// autoconfigs on the Zorro chain and carries its own scsi.device).
    pub scsi: ScsiConfig,
    pub floppy: FloppyConfig,
    /// Per-drive disk-swap playlists. Entry `i` is the ordered list of
    /// image paths configured for `dfI` (via `path`/`paths` in TOML); the
    /// first entry is the boot disk. A list with two or more entries lets
    /// the user cycle disks in that drive with the disk-swap key, so a
    /// multi-disk demo runs on a single drive. Empty for unused drives.
    pub floppy_playlists: [Vec<PathBuf>; 4],
    /// Presentation-level overscan handling for the window and
    /// screenshots (the emulated framebuffer always carries the full
    /// overscan field). See [`Overscan`].
    pub overscan: Overscan,
    /// CRT phosphor persistence: the fraction of the previous presented
    /// frame each new frame keeps (0.0 = off). Approximates the phosphor
    /// decay that fuses field-rate dither and interlace flicker on a
    /// real CRT.
    pub phosphor: f32,
}

/// How much of the overscan field the window presents. The
/// `COPPERLINE_OVERSCAN` env var (full/tv) overrides the config for one
/// run (the image-regression harness pins "full" so its baselines keep
/// the whole field).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Overscan {
    /// Present the full 716x285 overscan field the renderer produces
    /// (everything a real Denise can display).
    Full,
    /// Mask the deep-overscan margins with black, like a CRT bezel:
    /// only the standard PAL window (320x256 lo-res centred area)
    /// shows through. Demos often leave junk in the deep overscan
    /// (e.g. HAM streams converging off-screen); a real TV hides it
    /// behind the bezel, and so does this mode. The default.
    #[default]
    Tv,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IdeConfig {
    pub master: Option<PathBuf>,
    pub slave: Option<PathBuf>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScsiConfig {
    /// A2091/A590 boot ROM image (merged, or the even half when
    /// `rom_odd` gives the other EPROM).
    pub rom: Option<PathBuf>,
    /// Odd-byte EPROM half for split dumps.
    pub rom_odd: Option<PathBuf>,
    /// Drive images by SCSI ID (0-6; ID 7 is the controller).
    pub units: [Option<PathBuf>; 7],
}

impl ScsiConfig {
    /// Whether a `[scsi]` section asked for the board at all.
    pub fn enabled(&self) -> bool {
        self.rom.is_some() || self.units.iter().any(Option::is_some)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Emulation {
    /// Whether the machine starts running (powered on) at launch. When
    /// false, the emulator sits powered off showing a test screen until
    /// the status-bar power button is clicked -- handy for arming video
    /// capture beforehand. The power button cold-boots the machine.
    pub power_on: bool,
    /// How real-mode pacing debits its per-frame instruction budget. See
    /// `PacingBudget`. The `COPPERLINE_REAL_PACING_BUDGET` env var overrides
    /// this for one run.
    pub pacing_budget: PacingBudget,
    /// Ask the OS to schedule the latency-critical threads (the wall-clock
    /// pacer and the audio callback) above normal, to reduce stutter and audio
    /// glitches under host load. Best effort and off by default; see
    /// [`crate::priority`]. The `COPPERLINE_REALTIME_PRIORITY` env var
    /// overrides this for one run.
    pub realtime_priority: bool,
}

/// Real-mode pacing budget model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PacingBudget {
    /// Debit the budget by each instruction's actual returned m68k cycle
    /// count plus the chip-bus waits it incurred, clocking the CPU at its
    /// true cycles-per-instruction. The vendored core's 68000 cycle counts
    /// are now accurate (see `vendor/m68k/CYCLE_TIMING_GAP.md`), so this is
    /// the correct hardware-rate model and the default. (A separate
    /// blitter/raster-sync timing issue can make some area fills flicker
    /// under cycle pacing; tracked independently.)
    Cycles,
    /// Debit a flat `COPPERLINE_REAL_CPU_CPI` (default 4.0) cycles per retired
    /// instruction, regardless of the instruction's real cost. Cheaper and
    /// pacing-robust, but runs the CPU faster than hardware for instruction
    /// mixes that average more than the assumed flat cost. Opt in via
    /// `pacing_budget = "instructions"` or `COPPERLINE_REAL_PACING_BUDGET=instructions`.
    Instructions,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioConfig {
    /// Synthesized floppy-drive sound effects: motor hum, head-step
    /// clicks for seeks (and the empty-drive change-line poll), and a
    /// faint hiss while disk DMA moves data.
    pub floppy_sounds: bool,
    /// Drive sound level, 0-100, relative to Paula's output.
    pub floppy_sounds_volume: u8,
}

impl Default for AudioConfig {
    fn default() -> Self {
        Self {
            floppy_sounds: true,
            floppy_sounds_volume: 100,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FloppyConfig {
    pub drives: [Option<FloppyDriveConfig>; 4],
}

impl Default for FloppyConfig {
    fn default() -> Self {
        Self {
            drives: std::array::from_fn(|_| None),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FloppyDriveConfig {
    pub path: PathBuf,
    pub write_protected: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum CpuModel {
    M68000,
    M68EC020,
    M68020,
    M68030,
    M68040,
}

impl CpuModel {
    /// Whether the model ships with an FPU by default: the full 68040 has
    /// its floating-point unit on-die (the FPU-less variants are the LC/EC
    /// parts, which Copperline does not model); 68881/68882 boards for the
    /// other CPUs are opt-in via `[cpu] fpu = true`.
    pub fn default_fpu(self) -> bool {
        self == CpuModel::M68040
    }

    /// Default CPU clock in MHz for this model: a stock 68000/68010 runs at
    /// the PAL system clock (~7.09 MHz, 2x the colour clock); accelerated
    /// parts default to representative speeds (020 ~14 MHz, 030/040 ~25 MHz).
    /// Fast RAM runs at the CPU clock; chip/slow RAM stays chip-bus bound.
    pub fn default_clock_mhz(self) -> f64 {
        match self {
            CpuModel::M68000 => 7.09,
            CpuModel::M68EC020 | CpuModel::M68020 => 14.0,
            CpuModel::M68030 | CpuModel::M68040 => 25.0,
        }
    }

    /// Whether this model has the on-chip instruction cache Copperline models.
    /// The 68020/68EC020/68030 all ship a 256-byte direct-mapped instruction
    /// cache; AmigaOS enables it (CACR.EI) at boot. Real A1200/A4000 software
    /// (demos especially) leans on it: code looping out of chip RAM otherwise
    /// contends with bitplane DMA on every fetch and runs roughly half-speed.
    /// The 68040 caches exist but are not modelled here, so it reports false.
    pub fn has_instruction_cache(self) -> bool {
        matches!(
            self,
            CpuModel::M68EC020 | CpuModel::M68020 | CpuModel::M68030
        )
    }

    /// Whether this model has the on-chip data cache Copperline models. Only
    /// the 68030 has one (the 020 has none; the 040's is not modelled).
    pub fn has_data_cache(self) -> bool {
        self == CpuModel::M68030
    }
}

/// PAL Amiga colour clock (CCK), in Hz. The 68000 bus advances one slot per
/// CCK; the CPU runs at a whole multiple of it (2x for a stock 68000).
pub const COLOR_CLOCK_HZ: f64 = 3_546_895.0;

/// The CPU clock expressed as a whole multiple of the colour clock, clamped
/// to at least 1. A stock 68000 is 2 (7.09 MHz / 3.55 MHz); 14 MHz -> 4;
/// 25 MHz -> 7. The user can ask for any MHz; the chipset advance and pacing
/// model in whole CCK multiples ("multiples of the bus"), so the effective
/// clock is `clocks_per_cck * COLOR_CLOCK_HZ`.
pub fn clocks_per_cck_for_mhz(clock_mhz: f64) -> u32 {
    ((clock_mhz * 1.0e6) / COLOR_CLOCK_HZ).round().max(1.0) as u32
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Chipset {
    Ocs,
    Ecs,
    Aga,
}

/// `[machine] profile`: a validated bundle of chipset revisions, CPU model and
/// clock, memory sizes, RTC presence, and gate array. Explicit `[cpu]`/
/// `[chipset]`/`[memory]` sections override the profile defaults where
/// compatible; the profile owns what those sections cannot express (Gayle,
/// RTC presence). With no `[machine]` section the defaults are the legacy
/// A500-like behaviour, so existing configs are untouched.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum MachineModel {
    A500,
    A500Plus,
    A600,
    A1200,
    /// CDTV: an OCS A500-class machine with 1 MB chip RAM and the 256 KiB
    /// extended ROM at $F00000. Enables the DMAC/CD-ROM controller used by
    /// the CDTV drive.
    Cdtv,
    /// CD32: AGA, 68EC020, 2 MB chip RAM, Akiko at $B80000, and the
    /// 512 KiB extended ROM at $E00000. Enables Akiko and the CD32 CD-ROM
    /// path.
    Cd32,
}

/// Identity of a ROM image: its length and a CRC-32 of its bytes. Enough to
/// tell two Kickstarts apart (a different revision, or a CDTV/CD32 extended
/// ROM) without storing the image itself. The CRC is the standard IEEE
/// polynomial via `flate2::Crc`, so it is stable across builds and platforms.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RomId {
    pub len: usize,
    pub crc32: u32,
}

impl RomId {
    /// Fingerprint a ROM image. The empty slice gives `len 0`, which callers
    /// use to mean "no such ROM".
    pub fn of(bytes: &[u8]) -> Self {
        let mut crc = flate2::Crc::new();
        crc.update(bytes);
        Self {
            len: bytes.len(),
            crc32: crc.sum(),
        }
    }

    /// Compact label for logs/summaries, e.g. "512K:a1b2c3d4".
    pub fn label(&self) -> String {
        format!("{}K:{:08x}", self.len / 1024, self.crc32)
    }
}

/// The "shape" of a machine plus its ROM identity: the values that, taken
/// together, decide what kind of Amiga is running and which Kickstart it runs.
/// Embedded in the save-state header so a load can tell whether the state
/// belongs to a different machine than the running config and reconfigure the
/// host to match it.
///
/// The serialized `Bus`/`CpuCore` already carry the actual hardware (RAM
/// contents, ROM bytes, chip revisions, CPU type), so a state always rebuilds
/// its own machine on load; this descriptor is the compact, human-readable
/// identity used for the comparison and the log message, plus the machine
/// profile (`A500`/`A1200`/...), which is a config-level concept the Bus does
/// not record. The ROM fields fingerprint the boot/extended ROM bytes so a
/// swapped Kickstart of the same machine shape is still flagged.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct MachineDescriptor {
    pub cpu: CpuModel,
    pub chip_ram_bytes: usize,
    pub fast_ram_bytes: usize,
    pub slow_ram_bytes: usize,
    pub chipset: Chipset,
    pub video_standard: VideoStandard,
    pub machine: Option<MachineModel>,
    /// Boot ROM identity (the normalized in-memory image).
    pub rom: RomId,
    /// Extended ROM identity (CDTV $F00000 / CD32 $E00000), `None` when none
    /// is fitted.
    pub extended_rom: Option<RomId>,
}

impl Default for MachineDescriptor {
    /// A stock OCS A500 with no ROM fingerprint yet: the shape of the minimal
    /// machine the headless test fixtures build. Real runs overwrite this from
    /// the loaded `Config` and the in-memory ROM.
    fn default() -> Self {
        Self {
            cpu: CpuModel::M68000,
            chip_ram_bytes: 512 * 1024,
            fast_ram_bytes: 0,
            slow_ram_bytes: 0,
            chipset: Chipset::Ocs,
            video_standard: VideoStandard::Pal,
            machine: None,
            rom: RomId::default(),
            extended_rom: None,
        }
    }
}

impl MachineDescriptor {
    /// Fill the ROM fields from the live in-memory images. `extended_rom` is an
    /// empty slice when no extended ROM is fitted. Called once the machine is
    /// built (the bytes live in the `Bus`, not the `Config`).
    pub fn set_rom_fingerprint(&mut self, rom: &[u8], extended_rom: &[u8]) {
        self.rom = RomId::of(rom);
        self.extended_rom = (!extended_rom.is_empty()).then(|| RomId::of(extended_rom));
    }

    /// One-line human summary, e.g.
    /// "A1200 / 68EC020 / AGA / PAL / chip 2048K fast 0K slow 0K / ROM 512K:a1b2c3d4".
    pub fn summary(&self) -> String {
        let profile = match self.machine {
            Some(m) => format!("{m:?}"),
            None => "custom".to_string(),
        };
        let ext = match &self.extended_rom {
            Some(id) => format!(" +ext {}", id.label()),
            None => String::new(),
        };
        format!(
            "{profile} / {:?} / {:?} / {:?} / chip {}K fast {}K slow {}K / ROM {}{ext}",
            self.cpu,
            self.chipset,
            self.video_standard,
            self.chip_ram_bytes / 1024,
            self.fast_ram_bytes / 1024,
            self.slow_ram_bytes / 1024,
            self.rom.label(),
        )
    }

    /// Human-readable, field-by-field differences between the running machine
    /// (`self`) and a state's machine (`other`), for the load-time log when
    /// they do not match. Empty when the shapes and ROMs are identical.
    pub fn differences(&self, other: &MachineDescriptor) -> Vec<String> {
        let mut diffs = Vec::new();
        if self.machine != other.machine {
            diffs.push(format!("profile {:?} -> {:?}", self.machine, other.machine));
        }
        if self.cpu != other.cpu {
            diffs.push(format!("cpu {:?} -> {:?}", self.cpu, other.cpu));
        }
        if self.chipset != other.chipset {
            diffs.push(format!("chipset {:?} -> {:?}", self.chipset, other.chipset));
        }
        if self.video_standard != other.video_standard {
            diffs.push(format!(
                "video {:?} -> {:?}",
                self.video_standard, other.video_standard
            ));
        }
        if self.chip_ram_bytes != other.chip_ram_bytes {
            diffs.push(format!(
                "chip RAM {}K -> {}K",
                self.chip_ram_bytes / 1024,
                other.chip_ram_bytes / 1024
            ));
        }
        if self.fast_ram_bytes != other.fast_ram_bytes {
            diffs.push(format!(
                "fast RAM {}K -> {}K",
                self.fast_ram_bytes / 1024,
                other.fast_ram_bytes / 1024
            ));
        }
        if self.slow_ram_bytes != other.slow_ram_bytes {
            diffs.push(format!(
                "slow RAM {}K -> {}K",
                self.slow_ram_bytes / 1024,
                other.slow_ram_bytes / 1024
            ));
        }
        if self.rom != other.rom {
            diffs.push(format!("ROM {} -> {}", self.rom.label(), other.rom.label()));
        }
        if self.extended_rom != other.extended_rom {
            let label = |id: &Option<RomId>| match id {
                Some(id) => id.label(),
                None => "none".to_string(),
            };
            diffs.push(format!(
                "extended ROM {} -> {}",
                label(&self.extended_rom),
                label(&other.extended_rom)
            ));
        }
        diffs
    }
}

/// Which gate array the machine carries. Gayle owns IDE, PCMCIA, and the
/// interrupt plumbing at $DA8000-$DAA000 plus the ID register at $DE1000.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum GateArray {
    #[default]
    None,
    GayleA600,
    GayleA1200,
}

impl GateArray {
    /// The 8-bit ID shifted out of $DE1000 (MSB first): $D0 on the A600,
    /// $D1 on the A1200.
    pub fn gayle_id(self) -> Option<u8> {
        match self {
            Self::None => None,
            Self::GayleA600 => Some(0xD0),
            Self::GayleA1200 => Some(0xD1),
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            rom_path: PathBuf::from(BUNDLED_AROS_ROM),
            cpu: CpuModel::M68000,
            fpu: CpuModel::M68000.default_fpu(),
            cpu_clock_mhz: CpuModel::M68000.default_clock_mhz(),
            cpu_icache: false,
            cpu_dcache: false,
            emulation: Emulation {
                power_on: true,
                pacing_budget: PacingBudget::Cycles,
                realtime_priority: false,
            },
            chip_ram_bytes: 512 * 1024,
            fast_ram_bytes: 0,
            slow_ram_bytes: 0,
            z3_ram_bytes: 0,
            zorro_boards: Vec::new(),
            chipset: Chipset::Ocs,
            agnus_revision: AgnusRevision::Ocs,
            denise_revision: DeniseRevision::Ocs,
            machine: None,
            gate_array: GateArray::None,
            akiko: false,
            cdtv_cd: false,
            cd32_pad: false,
            extended_rom_path: None,
            cd_image_path: None,
            cd_insert_delay_secs: 0.0,
            cd32_nvram_path: None,
            rtc_present: true,
            video_standard: VideoStandard::Pal,
            audio: AudioConfig::default(),
            ide: IdeConfig::default(),
            scsi: ScsiConfig::default(),
            floppy: FloppyConfig::default(),
            floppy_playlists: std::array::from_fn(|_| Vec::new()),
            overscan: Overscan::Tv,
            phosphor: 0.0,
        }
    }
}

impl Config {
    /// Load a config, applying command-line overrides on top of whatever the
    /// file (or the built-in defaults, when `path` is `None`) provides. The
    /// overrides are injected into the raw TOML view before validation, so
    /// they go through exactly the same profile-defaulting, derivation, and
    /// range-checking as the equivalent config fields would.
    pub fn load_with_overrides(path: Option<&Path>, overrides: &ConfigOverrides) -> Result<Self> {
        let mut raw = match path {
            Some(p) => raw_from_path(p)?,
            None => RawConfig::default(),
        };
        overrides.apply_to(&mut raw);
        raw.try_into()
    }

    /// Apply a CLI ROM-path override on top of whatever the config
    /// produced. None leaves the config's value untouched.
    pub fn with_rom_override(mut self, rom: Option<PathBuf>) -> Self {
        if let Some(p) = rom {
            self.rom_path = p;
        }
        self
    }

    /// The machine "shape" this config describes, stamped into save states so
    /// a load can detect a different machine and reconfigure the host to match.
    /// The ROM fields are left empty here (the `Config` holds only a path); the
    /// caller fills them from the in-memory ROM via
    /// [`MachineDescriptor::set_rom_fingerprint`] once the machine is built.
    pub fn descriptor(&self) -> MachineDescriptor {
        MachineDescriptor {
            cpu: self.cpu,
            chip_ram_bytes: self.chip_ram_bytes,
            fast_ram_bytes: self.fast_ram_bytes,
            slow_ram_bytes: self.slow_ram_bytes,
            chipset: self.chipset,
            video_standard: self.video_standard,
            machine: self.machine,
            rom: RomId::default(),
            extended_rom: None,
        }
    }

    /// Build the Zorro autoconfig chain this config asks for: the built-in
    /// Zorro II fast RAM board, the built-in Zorro III RAM board, then any
    /// `[[zorro]]` metadata boards in file order.
    pub fn build_zorro_chain(&self) -> Result<ZorroChain> {
        let mut chain = ZorroChain::default();
        if self.fast_ram_bytes > 0 {
            chain.add_board(BoardSpec::fast_ram(self.fast_ram_bytes))?;
        }
        if self.z3_ram_bytes > 0 {
            chain.add_board(BoardSpec::z3_ram(self.z3_ram_bytes))?;
        }
        for board in &self.zorro_boards {
            chain.add_board(board.clone())?;
        }
        Ok(chain)
    }
}

/// Read and parse a config file into its raw TOML view.
fn raw_from_path(path: &Path) -> Result<RawConfig> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading config {}", path.display()))?;
    toml::from_str(&text).map_err(|e| {
        let mut err = anyhow::Error::new(e);
        // A backslash in a double-quoted TOML string is an escape character,
        // so a Windows path like "C:\Kickstarts\KICK31.ROM" fails to parse on
        // "\K". The bare "invalid escape sequence" message rarely makes that
        // connection, so point at the fix.
        if err.to_string().contains("escape") {
            err = err.context(
                "a backslash in a double-quoted string is an escape character; \
                 for Windows paths use single quotes ('C:\\dir\\file'), double \
                 the backslashes, or use forward slashes",
            );
        }
        err.context(format!("parsing config {}", path.display()))
    })
}

/// Command-line overrides for the handful of machine knobs it is convenient
/// to set without writing a config file: the machine model, the chipset
/// preset, the CPU and its FPU/clock, and the chip/fast RAM sizes. Each field
/// is `None` when the corresponding flag was not given, leaving the file (or
/// profile default) value untouched. The string fields carry the same syntax
/// the matching TOML fields accept and are validated by the same parsers.
#[derive(Debug, Default, Clone)]
pub struct ConfigOverrides {
    pub model: Option<String>,
    pub chipset: Option<String>,
    pub cpu: Option<String>,
    pub fpu: Option<bool>,
    pub cpu_clock_mhz: Option<f64>,
    pub chip: Option<String>,
    pub fast: Option<String>,
    pub slow: Option<String>,
}

impl ConfigOverrides {
    /// Whether any override was set.
    pub fn is_empty(&self) -> bool {
        self.model.is_none()
            && self.chipset.is_none()
            && self.cpu.is_none()
            && self.fpu.is_none()
            && self.cpu_clock_mhz.is_none()
            && self.chip.is_none()
            && self.fast.is_none()
            && self.slow.is_none()
    }

    /// Inject the set overrides into the raw config, replacing the values
    /// the file (or its absence) provided. Conversion validates the result.
    fn apply_to(&self, raw: &mut RawConfig) {
        if let Some(model) = &self.model {
            raw.machine.profile = Some(model.clone());
        }
        if let Some(chipset) = &self.chipset {
            raw.chipset.revision = Some(chipset.clone());
        }
        if let Some(cpu) = &self.cpu {
            raw.cpu.model = Some(cpu.clone());
        }
        if let Some(fpu) = self.fpu {
            raw.cpu.fpu = Some(fpu);
        }
        if let Some(mhz) = self.cpu_clock_mhz {
            raw.cpu.clock_mhz = Some(mhz);
        }
        if let Some(chip) = &self.chip {
            raw.memory.chip = Some(chip.clone());
        }
        if let Some(fast) = &self.fast {
            raw.memory.fast = Some(fast.clone());
        }
        if let Some(slow) = &self.slow {
            raw.memory.slow = Some(slow.clone());
        }
    }
}

// --- raw deserialization (one nested struct per [section]) ---------------

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    rom: Option<String>,
    /// Extended ROM image (CD32 512K at $E00000, CDTV 256K at $F00000).
    extended_rom: Option<String>,
    #[serde(default)]
    cd: RawCd,
    #[serde(default)]
    cpu: RawCpu,
    #[serde(default)]
    emulation: RawEmulation,
    #[serde(default)]
    memory: RawMemory,
    #[serde(default)]
    machine: RawMachine,
    #[serde(default)]
    chipset: RawChipset,
    #[serde(default)]
    audio: RawAudio,
    #[serde(default)]
    ide: RawIde,
    #[serde(default)]
    scsi: RawScsi,
    #[serde(default)]
    floppy: RawFloppy,
    #[serde(default)]
    display: RawDisplay,
    /// `[[zorro]]` board entries, configured in file order.
    #[serde(default)]
    zorro: Vec<RawZorroBoard>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawDisplay {
    /// "tv" (default, mask deep overscan like a CRT bezel) or "full".
    overscan: Option<String>,
    /// CRT phosphor persistence fraction, 0.0 (off, default) to 0.95.
    phosphor: Option<f32>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawIde {
    master: Option<String>,
    slave: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawScsi {
    /// A2091/A590 boot ROM image. For split even/odd EPROM dumps, `rom`
    /// is the even half and `rom_odd` the odd half.
    rom: Option<String>,
    rom_odd: Option<String>,
    unit0: Option<String>,
    unit1: Option<String>,
    unit2: Option<String>,
    unit3: Option<String>,
    unit4: Option<String>,
    unit5: Option<String>,
    unit6: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawCd {
    /// Path to a cue sheet (BIN/CUE).
    image: Option<String>,
    /// Insert the disc this many emulated seconds after power-on
    /// instead of at boot (CDTV; some discs only boot when inserted
    /// after the boot screen).
    insert_delay: Option<f64>,
    /// CD32 NVRAM (save game EEPROM) backing file. Defaults to
    /// "cd32-nvram.bin" on CD32 machines.
    nvram: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawCpu {
    model: Option<String>,
    fpu: Option<bool>,
    /// Override the CPU clock in MHz. Defaults to the model's stock speed.
    clock_mhz: Option<f64>,
    /// Model the on-chip instruction cache (68020/030; default false).
    icache: Option<bool>,
    /// Model the on-chip data cache (68030 only; default false).
    dcache: Option<bool>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawEmulation {
    /// Deprecated and ignored: "real" was the only remaining timing model,
    /// so the option carried no information. Still accepted (and warned
    /// about) so existing configs that name it keep parsing.
    speed: Option<String>,
    power_on: Option<bool>,
    pacing_budget: Option<String>,
    /// Best-effort realtime-like thread priority for the pacer and audio
    /// threads (default false). See `src/priority.rs`.
    realtime_priority: Option<bool>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawMemory {
    chip: Option<String>,
    fast: Option<String>,
    slow: Option<String>,
    /// Zorro III autoconfig RAM size (e.g. "16M"); 32-bit CPUs only.
    z3: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawZorroBoard {
    /// Path to a TOML board metadata file (see `src/zorro.rs` for the
    /// schema), resolved relative to the working directory.
    metadata: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawMachine {
    /// Machine profile name. Named `profile` (not `model`) so it never
    /// collides with `[cpu] model`: an uncommented profile line landing in
    /// the wrong table would otherwise be a confusing duplicate-key error.
    /// `model` stays accepted as a deprecated alias for old configs.
    #[serde(alias = "model")]
    profile: Option<String>,
    /// Whether the $DC0000 RTC is fitted; defaults per profile (the base
    /// A600 had none, the A600HD did -- default keeps it fitted so the
    /// guest OS clock works).
    rtc: Option<bool>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawChipset {
    revision: Option<String>,
    video: Option<String>,
    /// Fine-grained chip overrides on top of the `revision` preset, for the
    /// mixed machines that really shipped (e.g. late A500: ECS Agnus with an
    /// OCS Denise). `agnus` accepts OCS / 8370 / 8371 / 8372 / 8372A / 8372B /
    /// 8374 / 8375 / ALICE; `denise` accepts OCS / 8362 / ECS / 8373 / LISA /
    /// 4203.
    agnus: Option<String>,
    denise: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawAudio {
    floppy_sounds: Option<bool>,
    floppy_sounds_volume: Option<u16>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawFloppy {
    df0: Option<RawFloppyDrive>,
    df1: Option<RawFloppyDrive>,
    df2: Option<RawFloppyDrive>,
    df3: Option<RawFloppyDrive>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawFloppyDrive {
    enabled: Option<bool>,
    path: Option<String>,
    /// A playlist of images for this drive, cycled with the disk-swap
    /// key. When given, the first entry is the boot disk. May be used
    /// instead of `path`; if both appear, `path` is treated as the first
    /// entry followed by `paths`.
    paths: Option<Vec<String>>,
    write_protected: Option<bool>,
}

impl TryFrom<RawConfig> for Config {
    type Error = anyhow::Error;

    fn try_from(raw: RawConfig) -> Result<Self> {
        let machine = match raw.machine.profile.as_deref() {
            None => None,
            Some(s) => Some(parse_machine_model(s)?),
        };
        let defaults = machine.map_or_else(Config::default, machine_profile_defaults);
        let cpu = match raw.cpu.model.as_deref() {
            None => defaults.cpu,
            Some(s) => parse_cpu(s)?,
        };
        let fpu = raw.cpu.fpu.unwrap_or_else(|| cpu.default_fpu());
        let cpu_clock_mhz = match raw.cpu.clock_mhz {
            Some(mhz) if mhz.is_finite() && mhz > 0.0 => mhz,
            Some(_) => bail!("[cpu] clock_mhz must be a positive number"),
            None => cpu.default_clock_mhz(),
        };
        if fpu && cpu == CpuModel::M68000 {
            bail!(
                "[cpu] fpu = true needs the 68020+ coprocessor interface; \
                 a 68000 cannot drive a 68881/68882"
            );
        }
        // The on-chip caches are silicon: model them by default whenever the
        // CPU has them (AmigaOS turns them on via CACR), so a 020/030 matches
        // real hardware instead of contending with chip-bus DMA on every
        // instruction fetch. `[cpu] icache`/`dcache` still force either way.
        let cpu_icache = raw
            .cpu
            .icache
            .unwrap_or_else(|| cpu.has_instruction_cache());
        let cpu_dcache = raw.cpu.dcache.unwrap_or_else(|| cpu.has_data_cache());
        if cpu_icache
            && !matches!(
                cpu,
                CpuModel::M68EC020 | CpuModel::M68020 | CpuModel::M68030
            )
        {
            bail!(
                "[cpu] icache = true needs a 68020/68EC020/68030 (the 68000 has \
                 no cache; the 68040 cache is not modelled)"
            );
        }
        if cpu_dcache && cpu != CpuModel::M68030 {
            bail!("[cpu] dcache = true needs a 68030 (the 68020 has no data cache)");
        }
        if let Some(speed) = raw.emulation.speed.as_deref() {
            log::warn!(
                "[emulation] speed = {speed:?} is deprecated and ignored: the \
                 deterministic cycle-driven core is the only timing model"
            );
        }
        let emulation = Emulation {
            power_on: raw
                .emulation
                .power_on
                .unwrap_or(defaults.emulation.power_on),
            pacing_budget: match raw.emulation.pacing_budget.as_deref() {
                None => defaults.emulation.pacing_budget,
                Some(s) => parse_pacing_budget(s)?,
            },
            realtime_priority: raw
                .emulation
                .realtime_priority
                .unwrap_or(defaults.emulation.realtime_priority),
        };
        let chip_ram_bytes = match raw.memory.chip.as_deref() {
            None => defaults.chip_ram_bytes,
            Some(s) => parse_size(s, "chip RAM")?,
        };
        let fast_ram_bytes = match raw.memory.fast.as_deref() {
            None => defaults.fast_ram_bytes,
            Some(s) => parse_size(s, "fast RAM")?,
        };
        let slow_ram_bytes = match raw.memory.slow.as_deref() {
            None => defaults.slow_ram_bytes,
            Some(s) => parse_size(s, "slow RAM")?,
        };
        let z3_ram_bytes = match raw.memory.z3.as_deref() {
            None => defaults.z3_ram_bytes,
            Some(s) => parse_size(s, "Zorro III RAM")?,
        };
        let mut zorro_boards = Vec::new();
        for entry in &raw.zorro {
            zorro_boards.push(crate::zorro::load_board_metadata(Path::new(
                &entry.metadata,
            ))?);
        }
        let chipset = match raw.chipset.revision.as_deref() {
            None => defaults.chipset,
            Some(s) => parse_chipset(s)?,
        };
        let video_standard = match raw.chipset.video.as_deref() {
            None => defaults.video_standard,
            Some(s) => parse_video_standard(s)?,
        };
        let audio = AudioConfig {
            floppy_sounds: raw
                .audio
                .floppy_sounds
                .unwrap_or(defaults.audio.floppy_sounds),
            floppy_sounds_volume: match raw.audio.floppy_sounds_volume {
                None => defaults.audio.floppy_sounds_volume,
                Some(v) if v <= 100 => v as u8,
                Some(v) => bail!("[audio] floppy_sounds_volume must be 0-100, got {v}"),
            },
        };
        let (floppy, floppy_playlists) = parse_floppy(raw.floppy)?;
        let overscan = match raw.display.overscan.as_deref() {
            None => defaults.overscan,
            Some(s) => parse_overscan(s)?,
        };
        let phosphor = match raw.display.phosphor {
            None => defaults.phosphor,
            Some(p) if (0.0..=0.95).contains(&p) => p,
            Some(p) => bail!("[display] phosphor must be between 0.0 and 0.95, got {p}"),
        };

        let ide = IdeConfig {
            master: raw.ide.master.map(PathBuf::from),
            slave: raw.ide.slave.map(PathBuf::from),
        };
        if (ide.master.is_some() || ide.slave.is_some()) && defaults.gate_array == GateArray::None {
            bail!("[ide] images need a Gayle machine: set [machine] profile = \"A600\" (or A1200)");
        }

        let scsi = ScsiConfig {
            rom: raw.scsi.rom.map(PathBuf::from),
            rom_odd: raw.scsi.rom_odd.map(PathBuf::from),
            units: [
                raw.scsi.unit0.map(PathBuf::from),
                raw.scsi.unit1.map(PathBuf::from),
                raw.scsi.unit2.map(PathBuf::from),
                raw.scsi.unit3.map(PathBuf::from),
                raw.scsi.unit4.map(PathBuf::from),
                raw.scsi.unit5.map(PathBuf::from),
                raw.scsi.unit6.map(PathBuf::from),
            ],
        };
        if scsi.enabled() && scsi.rom.is_none() {
            bail!(
                "[scsi] drives need the A2091 boot ROM: set [scsi] rom = \"...\" \
                 (an A590/A2091 6.x ROM image; its scsi.device drives the disks)"
            );
        }
        if scsi.rom_odd.is_some() && scsi.rom.is_none() {
            bail!("[scsi] rom_odd needs rom (the even EPROM half)");
        }

        let agnus_revision = match raw.chipset.agnus.as_deref() {
            // The A600 board carries the 2 MB-capable 8375 regardless of
            // fitted chip RAM; other machines pick by preset + RAM size
            // (the A1200's AGA preset resolves to Alice).
            None => match machine {
                Some(MachineModel::A600) => AgnusRevision::Ecs8375,
                _ => default_agnus_revision(chipset, chip_ram_bytes),
            },
            Some(s) => parse_agnus_revision(s)?,
        };
        let denise_revision = match raw.chipset.denise.as_deref() {
            None => default_denise_revision(chipset),
            Some(s) => parse_denise_revision(s)?,
        };

        validate_chip_ram(chip_ram_bytes, chipset, agnus_revision)?;
        validate_fast_ram(fast_ram_bytes, chip_ram_bytes)?;
        validate_slow_ram(slow_ram_bytes)?;
        validate_z3_ram(z3_ram_bytes, cpu)?;
        for board in &zorro_boards {
            if board.version == ZorroVersion::III && !cpu_has_32bit_bus(cpu) {
                bail!(
                    "zorro board {:?} is Zorro III, which needs a 32-bit CPU \
                     (68020/68030/68040); {:?} has a 24-bit address bus",
                    board.name,
                    cpu
                );
            }
        }

        Ok(Config {
            rom_path: raw.rom.map(PathBuf::from).unwrap_or(defaults.rom_path),
            cpu,
            fpu,
            cpu_clock_mhz,
            cpu_icache,
            cpu_dcache,
            emulation,
            chip_ram_bytes,
            fast_ram_bytes,
            slow_ram_bytes,
            z3_ram_bytes,
            zorro_boards,
            chipset,
            agnus_revision,
            denise_revision,
            machine,
            gate_array: defaults.gate_array,
            akiko: defaults.akiko,
            cdtv_cd: defaults.cdtv_cd,
            cd32_pad: defaults.cd32_pad,
            extended_rom_path: raw
                .extended_rom
                .map(PathBuf::from)
                .or(defaults.extended_rom_path),
            cd_image_path: raw.cd.image.map(PathBuf::from),
            cd_insert_delay_secs: match raw.cd.insert_delay {
                Some(secs) if secs.is_finite() && secs >= 0.0 => secs,
                Some(_) => bail!("[cd] insert_delay must be a non-negative number"),
                None => 0.0,
            },
            cd32_nvram_path: raw
                .cd
                .nvram
                .map(PathBuf::from)
                .or_else(|| defaults.akiko.then(|| PathBuf::from("cd32-nvram.bin"))),
            rtc_present: raw.machine.rtc.unwrap_or(defaults.rtc_present),
            video_standard,
            audio,
            ide,
            scsi,
            floppy,
            floppy_playlists,
            overscan,
            phosphor,
        })
    }
}

pub(crate) fn parse_overscan(s: &str) -> Result<Overscan> {
    match s.trim().to_ascii_lowercase().as_str() {
        "full" => Ok(Overscan::Full),
        "tv" => Ok(Overscan::Tv),
        other => bail!("[display] overscan must be \"full\" or \"tv\", got \"{other}\""),
    }
}

fn parse_pacing_budget(s: &str) -> Result<PacingBudget> {
    match s.trim().to_ascii_lowercase().as_str() {
        "cycles" | "m68k-cycles" => Ok(PacingBudget::Cycles),
        "instructions" | "retired-instructions" => Ok(PacingBudget::Instructions),
        _ => Err(anyhow!(
            "unknown emulation pacing_budget {:?}: expected \"cycles\" or \"instructions\"",
            s
        )),
    }
}

fn parse_cpu(s: &str) -> Result<CpuModel> {
    let norm = s.trim().to_ascii_lowercase().replace(['m', '_', '-'], "");
    match norm.as_str() {
        "68000" | "000" => Ok(CpuModel::M68000),
        "68ec020" | "ec020" => Ok(CpuModel::M68EC020),
        "68020" | "020" => Ok(CpuModel::M68020),
        "68030" | "030" => Ok(CpuModel::M68030),
        "68040" | "040" => Ok(CpuModel::M68040),
        "68060" | "060" => Err(anyhow!(
            "cpu model {:?} is not supported by the m68k backend: expected 68000 / 68EC020 / 68020 / 68030 / 68040",
            s
        )),
        _ => Err(anyhow!(
            "unknown cpu model {:?}: expected 68000 / 68EC020 / 68020 / 68030 / 68040",
            s
        )),
    }
}

fn parse_chipset(s: &str) -> Result<Chipset> {
    match s.trim().to_ascii_uppercase().as_str() {
        "OCS" => Ok(Chipset::Ocs),
        "ECS" => Ok(Chipset::Ecs),
        "AGA" => Ok(Chipset::Aga),
        _ => Err(anyhow!("unknown chipset {:?}: expected OCS / ECS / AGA", s)),
    }
}

fn parse_machine_model(s: &str) -> Result<MachineModel> {
    let norm = s.trim().to_ascii_uppercase().replace(['_', '-', ' '], "");
    match norm.as_str() {
        "A500" => Ok(MachineModel::A500),
        "A500PLUS" | "A500+" => Ok(MachineModel::A500Plus),
        "A600" => Ok(MachineModel::A600),
        "A1200" => Ok(MachineModel::A1200),
        "CDTV" => Ok(MachineModel::Cdtv),
        "CD32" => Ok(MachineModel::Cd32),
        _ => Err(anyhow!(
            "unknown machine model {:?}: expected A500 / A500Plus / A600 / A1200 / CDTV / CD32",
            s
        )),
    }
}

/// The defaults a `[machine] profile` supplies before the explicit
/// `[cpu]`/`[chipset]`/`[memory]` sections override them.
fn machine_profile_defaults(model: MachineModel) -> Config {
    let mut d = Config {
        machine: Some(model),
        ..Config::default()
    };
    match model {
        // The classic OCS A500: identical to the legacy defaults.
        MachineModel::A500 => {}
        MachineModel::A500Plus => {
            d.chipset = Chipset::Ecs;
            d.chip_ram_bytes = 1024 * 1024;
        }
        MachineModel::A600 => {
            d.chipset = Chipset::Ecs;
            d.chip_ram_bytes = 1024 * 1024;
            d.gate_array = GateArray::GayleA600;
        }
        MachineModel::A1200 => {
            d.chipset = Chipset::Aga;
            d.chip_ram_bytes = 2 * 1024 * 1024;
            d.cpu = CpuModel::M68EC020;
            d.cpu_clock_mhz = 14.18;
            d.gate_array = GateArray::GayleA1200;
        }
        // CDTV: A500-class board with the 1 MB ECS Agnus and 1 MB chip
        // RAM, plus the 256 KiB extended ROM at $F00000 (configure it via
        // extended_rom = "..."). No Gayle.
        MachineModel::Cdtv => {
            d.chipset = Chipset::Ecs;
            d.chip_ram_bytes = 1024 * 1024;
            d.cdtv_cd = true;
        }
        // CD32: AGA, 68EC020 at 14 MHz, 2 MB chip RAM, Akiko, and the
        // 512 KiB extended ROM at $E00000. No Gayle, no RTC.
        MachineModel::Cd32 => {
            d.chipset = Chipset::Aga;
            d.chip_ram_bytes = 2 * 1024 * 1024;
            d.cpu = CpuModel::M68EC020;
            d.cpu_clock_mhz = 14.18;
            d.akiko = true;
            d.rtc_present = false;
            d.cd32_pad = true;
        }
    }
    d
}

/// Preset to Agnus mapping: the ECS preset picks the 2 MB 8375 only when
/// more than 1 MB of chip RAM is fitted, so identification and DMA pointer
/// gating match what such a machine would really carry. AGA selects Alice.
fn default_agnus_revision(chipset: Chipset, chip_ram_bytes: usize) -> AgnusRevision {
    match chipset {
        Chipset::Ocs => AgnusRevision::Ocs,
        Chipset::Ecs => {
            if chip_ram_bytes > 1024 * 1024 {
                AgnusRevision::Ecs8375
            } else {
                AgnusRevision::Ecs8372Rev4
            }
        }
        Chipset::Aga => AgnusRevision::AgaAlice,
    }
}

fn default_denise_revision(chipset: Chipset) -> DeniseRevision {
    match chipset {
        Chipset::Ocs => DeniseRevision::Ocs,
        Chipset::Ecs => DeniseRevision::Ecs8373,
        Chipset::Aga => DeniseRevision::AgaLisa,
    }
}

fn parse_agnus_revision(s: &str) -> Result<AgnusRevision> {
    match s.trim().to_ascii_uppercase().as_str() {
        "OCS" | "8370" | "8371" => Ok(AgnusRevision::Ocs),
        "8372" | "8372A" => Ok(AgnusRevision::Ecs8372Rev4),
        "8375" | "8372B" => Ok(AgnusRevision::Ecs8375),
        "8374" | "ALICE" => Ok(AgnusRevision::AgaAlice),
        _ => Err(anyhow!(
            "unknown chipset agnus {:?}: expected OCS / 8370 / 8371 / 8372 / 8372A / 8375 / 8374 / ALICE",
            s
        )),
    }
}

fn parse_denise_revision(s: &str) -> Result<DeniseRevision> {
    match s.trim().to_ascii_uppercase().as_str() {
        "OCS" | "8362" => Ok(DeniseRevision::Ocs),
        "ECS" | "8373" => Ok(DeniseRevision::Ecs8373),
        "LISA" | "4203" => Ok(DeniseRevision::AgaLisa),
        _ => Err(anyhow!(
            "unknown chipset denise {:?}: expected OCS / 8362 / ECS / 8373 / LISA / 4203",
            s
        )),
    }
}

fn parse_video_standard(s: &str) -> Result<VideoStandard> {
    match s.trim().to_ascii_uppercase().as_str() {
        "PAL" => Ok(VideoStandard::Pal),
        "NTSC" => Ok(VideoStandard::Ntsc),
        _ => Err(anyhow!(
            "unknown chipset video {:?}: expected PAL / NTSC",
            s
        )),
    }
}

/// Parse a human size like "512K", "1M", "2 MiB" or a raw byte count.
fn parse_size(s: &str, what: &str) -> Result<usize> {
    let raw = s.trim();
    if raw.is_empty() {
        bail!("{} size is empty", what);
    }
    // Split into numeric prefix + unit suffix.
    let split = raw.find(|c: char| !c.is_ascii_digit()).unwrap_or(raw.len());
    let (num_str, unit_str) = raw.split_at(split);
    let n: u64 = num_str
        .parse()
        .with_context(|| format!("{} size {:?}: bad number", what, s))?;
    let unit = unit_str.trim().to_ascii_uppercase().replace("IB", "B");
    let bytes = match unit.as_str() {
        "" | "B" => n,
        "K" | "KB" => n * 1024,
        "M" | "MB" => n * 1024 * 1024,
        "G" | "GB" => n * 1024 * 1024 * 1024,
        _ => bail!("{} size {:?}: unknown unit {:?}", what, s, unit_str),
    };
    if bytes % 4096 != 0 {
        bail!("{} size {} bytes must be a multiple of 4 KiB", what, bytes);
    }
    Ok(bytes as usize)
}

fn validate_chip_ram(bytes: usize, chipset: Chipset, agnus: AgnusRevision) -> Result<()> {
    let max = match chipset {
        Chipset::Ocs => 512 * 1024,
        Chipset::Ecs => 2 * 1024 * 1024,
        Chipset::Aga => 2 * 1024 * 1024,
    };
    if bytes == 0 {
        bail!("chip RAM must be > 0");
    }
    if bytes > max {
        bail!(
            "chip RAM {} bytes exceeds {:?} chipset maximum of {} bytes",
            bytes,
            chipset,
            max
        );
    }
    let agnus_max = agnus.dma_addr_capability_mask() as usize + 1;
    if bytes > agnus_max {
        bail!(
            "chip RAM {} bytes exceeds the {:?} Agnus address reach of {} bytes",
            bytes,
            agnus,
            agnus_max
        );
    }
    Ok(())
}

fn validate_fast_ram(fast: usize, chip: usize) -> Result<()> {
    // Standard Zorro II auto-configured fast RAM sits at $00200000,
    // limited to 8 MiB. If chip RAM occupies that space (only happens
    // with 2 MiB chip RAM on ECS/AGA), there's nowhere to put it.
    const FAST_BASE: usize = 0x0020_0000;
    const FAST_LIMIT: usize = 8 * 1024 * 1024;
    if fast == 0 {
        return Ok(());
    }
    if chip > FAST_BASE {
        bail!("fast RAM > 0 incompatible with chip RAM > 2 MiB (no room at $00200000)");
    }
    if fast > FAST_LIMIT {
        bail!(
            "fast RAM {} bytes exceeds Zorro II maximum of {} bytes",
            fast,
            FAST_LIMIT
        );
    }
    if zorro_ii_size_code(fast).is_none() {
        bail!(
            "fast RAM {} bytes is not an autoconfigurable Zorro II size (64K, 128K, 256K, 512K, 1M, 2M, 4M, or 8M)",
            fast
        );
    }
    Ok(())
}

fn cpu_has_32bit_bus(cpu: CpuModel) -> bool {
    matches!(cpu, CpuModel::M68020 | CpuModel::M68030 | CpuModel::M68040)
}

fn validate_z3_ram(z3: usize, cpu: CpuModel) -> Result<()> {
    if z3 == 0 {
        return Ok(());
    }
    if !cpu_has_32bit_bus(cpu) {
        bail!(
            "Zorro III RAM needs a CPU with a 32-bit address bus \
             (68020/68030/68040); {:?} has a 24-bit bus",
            cpu
        );
    }
    if zorro_iii_size_bits(z3).is_none() {
        bail!(
            "Zorro III RAM {} bytes is not an autoconfigurable size \
             (a power of two from 64K to 1G)",
            z3
        );
    }
    Ok(())
}

fn validate_slow_ram(slow: usize) -> Result<()> {
    const SLOW_LIMIT: usize = 512 * 1024;
    if slow > SLOW_LIMIT {
        bail!(
            "slow RAM {} bytes exceeds A500 trapdoor/fake-fast maximum of {} bytes",
            slow,
            SLOW_LIMIT
        );
    }
    Ok(())
}

fn parse_floppy(raw: RawFloppy) -> Result<(FloppyConfig, [Vec<PathBuf>; 4])> {
    let raws = [raw.df0, raw.df1, raw.df2, raw.df3];
    let mut drives: [Option<FloppyDriveConfig>; 4] = std::array::from_fn(|_| None);
    let mut playlists: [Vec<PathBuf>; 4] = std::array::from_fn(|_| Vec::new());
    for (idx, raw_drive) in raws.into_iter().enumerate() {
        let Some(raw_drive) = raw_drive else {
            continue;
        };
        // Combine `path` (single) and `paths` (playlist) into one ordered
        // list, with `path` first when both are present.
        let mut raw_images: Vec<String> = Vec::new();
        if let Some(path) = raw_drive.path {
            raw_images.push(path);
        }
        if let Some(paths) = raw_drive.paths {
            raw_images.extend(paths);
        }
        let has_images = !raw_images.is_empty();
        let enabled = raw_drive.enabled.unwrap_or(has_images);
        if !enabled {
            continue;
        }
        if !has_images {
            bail!("floppy.df{} is enabled but has no path", idx);
        }
        let mut images = Vec::with_capacity(raw_images.len());
        for image in raw_images {
            if image.trim().is_empty() {
                bail!("floppy.df{} path is empty", idx);
            }
            let image = PathBuf::from(image);
            validate_floppy_image_path(idx, &image)?;
            images.push(image);
        }
        drives[idx] = Some(FloppyDriveConfig {
            path: images[0].clone(),
            write_protected: raw_drive.write_protected.unwrap_or(true),
        });
        playlists[idx] = images;
    }
    Ok((FloppyConfig { drives }, playlists))
}

fn validate_floppy_image_path(idx: usize, path: &Path) -> Result<()> {
    const ADF_SIZE: u64 = 80 * 2 * 11 * 512;
    let meta = std::fs::metadata(path)
        .with_context(|| format!("reading floppy.df{} image {}", idx, path.display()))?;
    if !meta.is_file() {
        bail!("floppy.df{} image {} is not a file", idx, path.display());
    }
    if meta.len() == ADF_SIZE {
        return Ok(());
    }

    let mut sig = [0u8; 8];
    let mut file = std::fs::File::open(path)
        .with_context(|| format!("opening floppy.df{} image {}", idx, path.display()))?;
    file.read_exact(&mut sig).with_context(|| {
        format!(
            "reading floppy.df{} image signature {}",
            idx,
            path.display()
        )
    })?;
    if sig[..2] == [0x1F, 0x8B]
        || &sig[..3] == b"SCP"
        || &sig[..4] == b"DMS!"
        || &sig == b"UAE-1ADF"
        || &sig == b"UAE--ADF"
    {
        return Ok(());
    }

    bail!(
        "floppy.df{} image {} is {} bytes, expected {} bytes (standard DD ADF), gzip-compressed supported image, UAE extended ADF, SCP, or DMS",
        idx,
        path.display(),
        meta.len(),
        ADF_SIZE
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn parse_config(text: &str) -> Result<Config> {
        let raw: RawConfig = toml::from_str(text)?;
        raw.try_into()
    }

    #[test]
    fn rom_fingerprint_distinguishes_same_shape_kickstarts() {
        // Two machines of identical shape but different boot ROMs must compare
        // as a mismatch (the whole point of fingerprinting the ROM rather than
        // only the machine shape).
        let mut a = MachineDescriptor::default();
        a.set_rom_fingerprint(b"kickstart 3.1 r40.068", b"");
        let mut b = MachineDescriptor::default();
        b.set_rom_fingerprint(b"kickstart 3.1.4 r46.143", b"");
        assert_ne!(a.rom, b.rom);
        let diffs = a.differences(&b);
        assert_eq!(diffs.len(), 1);
        assert!(diffs[0].starts_with("ROM "), "{diffs:?}");

        // The same image fingerprints identically, and an added extended ROM is
        // flagged on its own (same boot ROM, gained an extended ROM).
        let mut c = MachineDescriptor::default();
        c.set_rom_fingerprint(b"kickstart 3.1 r40.068", b"");
        assert_eq!(a.rom, c.rom);
        assert!(a.differences(&c).is_empty());
        let mut d = a.clone();
        d.set_rom_fingerprint(b"kickstart 3.1 r40.068", b"cd32 extended rom");
        let ext_diffs = a.differences(&d);
        assert_eq!(ext_diffs.len(), 1);
        assert!(
            ext_diffs[0].starts_with("extended ROM none -> "),
            "{ext_diffs:?}"
        );
    }

    #[test]
    fn windows_path_escape_error_explains_fix() {
        // A backslash in a double-quoted TOML string is an escape character,
        // so an unescaped Windows path fails on "\K". The error must point at
        // the remedy rather than leaving a bare "invalid escape sequence".
        let path = temp_path("badescape.toml");
        fs::write(&path, "rom = \"C:\\Kickstarts\\KICK31.ROM\"\n").unwrap();
        let err = raw_from_path(&path).unwrap_err();
        let _ = fs::remove_file(&path);
        let msg = format!("{err:#}");
        assert!(
            msg.contains("single quotes") && msg.contains("forward slashes"),
            "error should explain the Windows-path fix, got: {msg}"
        );
    }

    #[test]
    fn missing_emulation_uses_defaults() -> Result<()> {
        let cfg = parse_config("")?;
        assert!(cfg.emulation.power_on);
        assert_eq!(cfg.emulation.pacing_budget, PacingBudget::Cycles);
        Ok(())
    }

    #[test]
    fn deprecated_speed_option_is_accepted_and_ignored() -> Result<()> {
        // `[emulation] speed` was removed once "real" became the only timing
        // model. Any value is now tolerated (and warned about) so old configs
        // still parse, but it has no effect.
        for value in ["real", "turbo", "warp"] {
            parse_config(&format!("[emulation]\nspeed = {value:?}\n"))?;
        }
        Ok(())
    }

    #[test]
    fn power_on_defaults_to_true() -> Result<()> {
        let cfg = parse_config("")?;
        assert!(cfg.emulation.power_on);
        Ok(())
    }

    #[test]
    fn power_on_false_parses() -> Result<()> {
        let cfg = parse_config(
            r#"
            [emulation]
            power_on = false
            "#,
        )?;
        assert!(!cfg.emulation.power_on);
        Ok(())
    }

    #[test]
    fn display_overscan_parses_and_defaults_to_tv() -> Result<()> {
        assert_eq!(parse_config("")?.overscan, Overscan::Tv);
        let cfg = parse_config(
            r#"
            [display]
            overscan = "Full"
            "#,
        )?;
        assert_eq!(cfg.overscan, Overscan::Full);
        assert!(parse_config("[display]\noverscan = \"crop\"").is_err());
        Ok(())
    }

    #[test]
    fn display_phosphor_parses_and_rejects_out_of_range() -> Result<()> {
        assert_eq!(parse_config("")?.phosphor, 0.0);
        let cfg = parse_config(
            r#"
            [display]
            phosphor = 0.4
            "#,
        )?;
        assert_eq!(cfg.phosphor, 0.4);
        assert!(parse_config("[display]\nphosphor = 1.5").is_err());
        assert!(parse_config("[display]\nphosphor = -0.1").is_err());
        Ok(())
    }

    #[test]
    fn chipset_video_standard_parses() -> Result<()> {
        let cfg = parse_config(
            r#"
            [chipset]
            video = "NTSC"
            "#,
        )?;
        assert_eq!(cfg.video_standard, VideoStandard::Ntsc);
        Ok(())
    }

    #[test]
    fn machine_profiles_supply_defaults_and_keep_overrides() -> Result<()> {
        // No [machine] section: legacy defaults, no gate array, RTC fitted.
        let cfg = parse_config("")?;
        assert_eq!(cfg.machine, None);
        assert_eq!(cfg.gate_array, GateArray::None);
        assert!(cfg.rtc_present);

        let cfg = parse_config(
            r#"
            [machine]
            profile = "A600"
            "#,
        )?;
        assert_eq!(cfg.machine, Some(MachineModel::A600));
        assert_eq!(cfg.gate_array, GateArray::GayleA600);
        assert_eq!(cfg.chipset, Chipset::Ecs);
        assert_eq!(cfg.chip_ram_bytes, 1024 * 1024);
        // The A600 board carries the 2 MB-capable 8375 even with 1 MB fitted.
        assert_eq!(cfg.agnus_revision, AgnusRevision::Ecs8375);
        assert_eq!(cfg.denise_revision, DeniseRevision::Ecs8373);
        assert_eq!(cfg.cpu, CpuModel::M68000);
        assert!(cfg.rtc_present);

        // Explicit sections override profile defaults.
        let cfg = parse_config(
            r#"
            [machine]
            profile = "A600"
            rtc = false
            [memory]
            chip = "2M"
            "#,
        )?;
        assert_eq!(cfg.chip_ram_bytes, 2 * 1024 * 1024);
        assert!(!cfg.rtc_present);

        let cfg = parse_config(
            r#"
            [machine]
            profile = "A500Plus"
            "#,
        )?;
        assert_eq!(cfg.chipset, Chipset::Ecs);
        assert_eq!(cfg.chip_ram_bytes, 1024 * 1024);
        assert_eq!(cfg.agnus_revision, AgnusRevision::Ecs8372Rev4);
        assert_eq!(cfg.gate_array, GateArray::None);

        let cfg = parse_config(
            r#"
            [machine]
            profile = "A1200"
            "#,
        )?;
        assert_eq!(cfg.cpu, CpuModel::M68EC020);
        assert_eq!(cfg.gate_array, GateArray::GayleA1200);
        assert_eq!(cfg.agnus_revision, AgnusRevision::AgaAlice);
        assert_eq!(cfg.denise_revision, DeniseRevision::AgaLisa);

        let err = parse_config(
            r#"
            [machine]
            profile = "A4000"
            "#,
        )
        .unwrap_err();
        assert!(err.to_string().contains("unknown machine model"), "{err:#}");
        Ok(())
    }

    #[test]
    fn machine_profile_accepts_deprecated_model_alias() -> Result<()> {
        // `[machine] model` was the original key name; it now collides
        // visually with `[cpu] model`, so the canonical key is `profile`.
        // The old name stays accepted so existing configs keep working.
        let by_alias = parse_config(
            r#"
            [machine]
            model = "A1200"
            "#,
        )?;
        let by_profile = parse_config(
            r#"
            [machine]
            profile = "A1200"
            "#,
        )?;
        assert_eq!(by_alias.machine, Some(MachineModel::A1200));
        assert_eq!(by_alias.machine, by_profile.machine);
        Ok(())
    }

    #[test]
    fn ide_images_require_a_gayle_machine() {
        let err = parse_config(
            r#"
            [ide]
            master = "disk.hdf"
            "#,
        )
        .unwrap_err();
        assert!(err.to_string().contains("Gayle machine"), "{err:#}");

        let cfg = parse_config(
            r#"
            [machine]
            profile = "A600"
            [ide]
            master = "disk.hdf"
            "#,
        )
        .unwrap();
        assert_eq!(cfg.ide.master.as_deref(), Some(Path::new("disk.hdf")));
        assert_eq!(cfg.ide.slave, None);
    }

    #[test]
    fn ecs_preset_picks_agnus_variant_from_chip_ram() -> Result<()> {
        let cfg = parse_config(
            r#"
            [chipset]
            revision = "ECS"
            [memory]
            chip = "512K"
            "#,
        )?;
        assert_eq!(cfg.agnus_revision, AgnusRevision::Ecs8372Rev4);
        assert_eq!(cfg.denise_revision, DeniseRevision::Ecs8373);

        let cfg = parse_config(
            r#"
            [chipset]
            revision = "ECS"
            [memory]
            chip = "2M"
            "#,
        )?;
        assert_eq!(cfg.agnus_revision, AgnusRevision::Ecs8375);
        Ok(())
    }

    #[test]
    fn chipset_agnus_denise_overrides_parse() -> Result<()> {
        // Late-A500 mix: ECS Agnus with the original OCS Denise.
        let cfg = parse_config(
            r#"
            [chipset]
            revision = "ECS"
            denise = "OCS"
            "#,
        )?;
        assert_eq!(cfg.agnus_revision, AgnusRevision::Ecs8372Rev4);
        assert_eq!(cfg.denise_revision, DeniseRevision::Ocs);

        let cfg = parse_config(
            r#"
            [chipset]
            revision = "ECS"
            agnus = "8375"
            "#,
        )?;
        assert_eq!(cfg.agnus_revision, AgnusRevision::Ecs8375);

        let err = parse_config(
            r#"
            [chipset]
            agnus = "8378"
            "#,
        )
        .unwrap_err();
        assert!(err.to_string().contains("unknown chipset agnus"), "{err:#}");
        Ok(())
    }

    #[test]
    fn chip_ram_beyond_agnus_reach_is_rejected() {
        let err = parse_config(
            r#"
            [chipset]
            revision = "ECS"
            agnus = "8372A"
            [memory]
            chip = "2M"
            "#,
        )
        .unwrap_err();
        assert!(err.to_string().contains("Agnus address reach"), "{err:#}");
    }

    #[test]
    fn invalid_video_standard_fails_cleanly() {
        let err = parse_config(
            r#"
            [chipset]
            video = "SECAM"
            "#,
        )
        .unwrap_err();
        assert!(err.to_string().contains("unknown chipset video"), "{err:#}");
    }

    #[test]
    fn cpu_68ec020_parses_as_24_bit_020() -> Result<()> {
        let cfg = parse_config(
            r#"
            [cpu]
            model = "68EC020"
            "#,
        )?;
        assert_eq!(cfg.cpu, CpuModel::M68EC020);
        Ok(())
    }

    #[test]
    fn fpu_defaults_from_cpu_model() -> Result<()> {
        // 68881/68882 boards are opt-in on the 020/030...
        let cfg = parse_config(
            r#"
            [cpu]
            model = "68020"
            "#,
        )?;
        assert!(!cfg.fpu);

        // ...but the full 68040 has its FPU on-die.
        let cfg = parse_config(
            r#"
            [cpu]
            model = "68040"
            "#,
        )?;
        assert!(cfg.fpu);
        Ok(())
    }

    #[test]
    fn fpu_needs_the_coprocessor_interface() -> Result<()> {
        // A 68000 cannot drive a 68881/68882.
        let err = parse_config(
            r#"
            [cpu]
            model = "68000"
            fpu = true
            "#,
        )
        .unwrap_err();
        assert!(err.to_string().contains("coprocessor interface"));

        // Any 020+ can.
        let cfg = parse_config(
            r#"
            [cpu]
            model = "68EC020"
            fpu = true
            "#,
        )?;
        assert!(cfg.fpu);
        Ok(())
    }

    #[test]
    fn cpu_68060_is_rejected() {
        let err = parse_config(
            r#"
            [cpu]
            model = "68060"
            fpu = false
            "#,
        )
        .unwrap_err();
        assert!(err.to_string().contains("not supported"));
    }

    #[test]
    fn fast_ram_must_use_zorro_ii_autoconfig_size() {
        let err = parse_config(
            r#"
            [memory]
            fast = "768K"
            "#,
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("not an autoconfigurable"),
            "{err:#}"
        );
    }

    #[test]
    fn cpu_cache_flags_gate_on_model() -> Result<()> {
        let cfg = parse_config(
            r#"
            [cpu]
            model = "68030"
            icache = true
            dcache = true
            "#,
        )?;
        assert!(cfg.cpu_icache);
        assert!(cfg.cpu_dcache);

        // Caches default on for the silicon that has them: a 68020/68EC020
        // gets the instruction cache (no data cache); a 68030 gets both.
        let cfg = parse_config("[cpu]\nmodel = \"68020\"")?;
        assert!(cfg.cpu_icache && !cfg.cpu_dcache);
        let cfg = parse_config("[cpu]\nmodel = \"68030\"")?;
        assert!(cfg.cpu_icache && cfg.cpu_dcache);

        // A plain 68000 has neither.
        let cfg = parse_config("[cpu]\nmodel = \"68000\"")?;
        assert!(!cfg.cpu_icache && !cfg.cpu_dcache);

        // The default is overridable: a 020 can opt out of its instruction cache.
        let cfg = parse_config("[cpu]\nmodel = \"68020\"\nicache = false")?;
        assert!(!cfg.cpu_icache);

        let err = parse_config("[cpu]\nicache = true").unwrap_err();
        assert!(err.to_string().contains("icache"), "{err:#}");

        let err = parse_config(
            r#"
            [cpu]
            model = "68020"
            dcache = true
            "#,
        )
        .unwrap_err();
        assert!(err.to_string().contains("68030"), "{err:#}");
        Ok(())
    }

    #[test]
    fn z3_ram_needs_a_32_bit_cpu() {
        let err = parse_config(
            r#"
            [memory]
            z3 = "16M"
            "#,
        )
        .unwrap_err();
        assert!(err.to_string().contains("32-bit address bus"), "{err:#}");

        let err = parse_config(
            r#"
            [cpu]
            model = "68EC020"
            [memory]
            z3 = "16M"
            "#,
        )
        .unwrap_err();
        assert!(err.to_string().contains("32-bit address bus"), "{err:#}");
    }

    #[test]
    fn z3_ram_parses_with_32_bit_cpu_and_validates_size() -> Result<()> {
        let cfg = parse_config(
            r#"
            [cpu]
            model = "68030"
            [memory]
            z3 = "16M"
            "#,
        )?;
        assert_eq!(cfg.z3_ram_bytes, 16 * 1024 * 1024);

        let err = parse_config(
            r#"
            [cpu]
            model = "68030"
            [memory]
            z3 = "24M"
            "#,
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("not an autoconfigurable"),
            "{err:#}"
        );
        Ok(())
    }

    #[test]
    fn scsi_section_parses_units_and_requires_the_boot_rom() -> Result<()> {
        let cfg = parse_config(
            r#"
            [scsi]
            rom = "a2091.rom"
            unit0 = "workbench.hdf"
            unit3 = "data.hdf"
            "#,
        )?;
        assert!(cfg.scsi.enabled());
        assert_eq!(cfg.scsi.rom.as_deref(), Some(Path::new("a2091.rom")));
        assert_eq!(
            cfg.scsi.units[0].as_deref(),
            Some(Path::new("workbench.hdf"))
        );
        assert!(cfg.scsi.units[1].is_none());
        assert_eq!(cfg.scsi.units[3].as_deref(), Some(Path::new("data.hdf")));

        // Drives without the boot ROM cannot work: the ROM carries the
        // scsi.device driver.
        let err = parse_config(
            r#"
            [scsi]
            unit0 = "workbench.hdf"
            "#,
        )
        .unwrap_err();
        assert!(err.to_string().contains("boot ROM"), "{err:#}");

        // SCSI works on any machine model (no Gayle requirement). This also
        // exercises the deprecated `model` alias for `[machine] profile`.
        let cfg = parse_config(
            r#"
            [machine]
            model = "A500"
            [scsi]
            rom = "a2091.rom"
            unit0 = "dh0.hdf"
            "#,
        )?;
        assert!(cfg.scsi.enabled());
        Ok(())
    }

    #[test]
    fn zorro_metadata_boards_parse_and_gate_on_cpu() -> Result<()> {
        let meta = temp_path("board.toml");
        fs::write(
            &meta,
            r#"
            name = "MegaRAM"
            zorro = 3
            type = "ram"
            size = "32M"
            manufacturer = 2011
            product = 32
            "#,
        )?;

        let cfg = parse_config(&format!(
            r#"
            [cpu]
            model = "68030"
            [[zorro]]
            metadata = "{}"
            "#,
            toml_path(&meta)
        ))?;
        assert_eq!(cfg.zorro_boards.len(), 1);
        assert_eq!(cfg.zorro_boards[0].name, "MegaRAM");
        assert_eq!(cfg.zorro_boards[0].size_bytes, 32 * 1024 * 1024);

        let err = parse_config(&format!(
            r#"
            [[zorro]]
            metadata = "{}"
            "#,
            toml_path(&meta)
        ))
        .unwrap_err();
        assert!(err.to_string().contains("needs a 32-bit CPU"), "{err:#}");

        let _ = fs::remove_file(&meta);
        Ok(())
    }

    #[test]
    fn slow_ram_parses_for_a500_trapdoor_memory() -> Result<()> {
        let cfg = parse_config(
            r#"
            [memory]
            slow = "512K"
            "#,
        )?;
        assert_eq!(cfg.slow_ram_bytes, 512 * 1024);
        Ok(())
    }

    #[test]
    fn slow_ram_is_limited_to_trapdoor_size() {
        let err = parse_config(
            r#"
            [memory]
            slow = "1M"
            "#,
        )
        .unwrap_err();
        assert!(err.to_string().contains("slow RAM"), "{err:#}");
    }

    #[test]
    fn floppy_path_implies_enabled_and_write_protect_defaults() -> Result<()> {
        let adf = temp_adf()?;
        let cfg = parse_config(&format!(
            r#"
            [floppy.df0]
            path = "{}"
            "#,
            toml_path(&adf)
        ))?;
        let df0 = cfg.floppy.drives[0].as_ref().unwrap();
        assert_eq!(df0.path, adf);
        assert!(df0.write_protected);
        Ok(())
    }

    #[test]
    fn cpu_clock_defaults_per_model_and_converts_to_cck_multiple() {
        assert_eq!(CpuModel::M68000.default_clock_mhz(), 7.09);
        assert_eq!(CpuModel::M68020.default_clock_mhz(), 14.0);
        assert_eq!(CpuModel::M68040.default_clock_mhz(), 25.0);
        // Whole multiples of the colour clock ("multiples of the bus").
        assert_eq!(clocks_per_cck_for_mhz(7.09), 2);
        assert_eq!(clocks_per_cck_for_mhz(14.0), 4);
        assert_eq!(clocks_per_cck_for_mhz(25.0), 7);
        // Never zero.
        assert_eq!(clocks_per_cck_for_mhz(0.5), 1);
    }

    #[test]
    fn cpu_clock_override_is_honoured_and_validated() -> Result<()> {
        let cfg = parse_config(
            r#"
            [cpu]
            model = "68020"
            clock_mhz = 28.0
            "#,
        )?;
        assert_eq!(cfg.cpu, CpuModel::M68020);
        assert_eq!(cfg.cpu_clock_mhz, 28.0);
        // Default applies when unset.
        let cfg = parse_config(
            r#"[cpu]
            model = "68040""#,
        )?;
        assert_eq!(cfg.cpu_clock_mhz, 25.0);
        // Non-positive is rejected.
        assert!(parse_config("[cpu]\nclock_mhz = 0.0").is_err());
        Ok(())
    }

    #[test]
    fn floppy_paths_playlist_is_parsed_in_order() -> Result<()> {
        let disk1 = temp_adf()?;
        let disk2 = temp_adf()?;
        let cfg = parse_config(&format!(
            r#"
            [floppy.df0]
            paths = ["{}", "{}"]
            write_protected = false
            "#,
            toml_path(&disk1),
            toml_path(&disk2),
        ))?;
        // The boot disk is the first playlist entry.
        let df0 = cfg.floppy.drives[0].as_ref().unwrap();
        assert_eq!(df0.path, disk1);
        assert!(!df0.write_protected);
        // The full playlist is exposed in order for the swap key.
        assert_eq!(cfg.floppy_playlists[0], vec![disk1, disk2]);
        assert!(cfg.floppy_playlists[1].is_empty());
        Ok(())
    }

    #[test]
    fn floppy_single_path_yields_one_entry_playlist() -> Result<()> {
        let adf = temp_adf()?;
        let cfg = parse_config(&format!(
            r#"
            [floppy.df0]
            path = "{}"
            "#,
            toml_path(&adf)
        ))?;
        assert_eq!(cfg.floppy_playlists[0], vec![adf]);
        Ok(())
    }

    #[test]
    fn dms_floppy_path_is_accepted() -> Result<()> {
        let dms = temp_path("test.dms");
        fs::write(&dms, b"DMS!test placeholder")?;
        let cfg = parse_config(&format!(
            r#"
            [floppy.df0]
            path = "{}"
            "#,
            toml_path(&dms)
        ))?;
        let df0 = cfg.floppy.drives[0].as_ref().unwrap();
        assert_eq!(df0.path, dms);
        assert!(df0.write_protected);
        let _ = fs::remove_file(df0.path.clone());
        Ok(())
    }

    #[test]
    fn adz_floppy_path_is_accepted() -> Result<()> {
        let adz = temp_path("test.adz");
        fs::write(&adz, [0x1F, 0x8B, 8, 0, 0, 0, 0, 0])?;
        let cfg = parse_config(&format!(
            r#"
            [floppy.df0]
            path = "{}"
            "#,
            toml_path(&adz)
        ))?;
        let df0 = cfg.floppy.drives[0].as_ref().unwrap();
        assert_eq!(df0.path, adz);
        assert!(df0.write_protected);
        let _ = fs::remove_file(df0.path.clone());
        Ok(())
    }

    #[test]
    fn uae_extended_adf_floppy_path_is_accepted() -> Result<()> {
        let adf = temp_path("test.ext.adf");
        let mut image = Vec::new();
        image.extend_from_slice(b"UAE-1ADF");
        image.extend_from_slice(&0u16.to_be_bytes());
        image.extend_from_slice(&0u16.to_be_bytes());
        fs::write(&adf, image)?;
        let cfg = parse_config(&format!(
            r#"
            [floppy.df0]
            path = "{}"
            "#,
            toml_path(&adf)
        ))?;
        let df0 = cfg.floppy.drives[0].as_ref().unwrap();
        assert_eq!(df0.path, adf);
        let _ = fs::remove_file(df0.path.clone());
        Ok(())
    }

    #[test]
    fn scp_floppy_path_is_accepted() -> Result<()> {
        let scp = temp_path("test.scp");
        fs::write(&scp, b"SCP\x25\x04\x01\x00\x00")?;
        let cfg = parse_config(&format!(
            r#"
            [floppy.df0]
            path = "{}"
            "#,
            toml_path(&scp)
        ))?;
        let df0 = cfg.floppy.drives[0].as_ref().unwrap();
        assert_eq!(df0.path, scp);
        assert!(df0.write_protected);
        let _ = fs::remove_file(df0.path.clone());
        Ok(())
    }

    #[test]
    fn disabled_floppy_ignores_missing_path() -> Result<()> {
        let cfg = parse_config(
            r#"
            [floppy.df1]
            enabled = false
            "#,
        )?;
        assert!(cfg.floppy.drives[1].is_none());
        Ok(())
    }

    #[test]
    fn enabled_floppy_requires_path() {
        let err = parse_config(
            r#"
            [floppy.df0]
            enabled = true
            "#,
        )
        .unwrap_err();
        assert!(err.to_string().contains("has no path"), "{err:#}");
    }

    #[test]
    fn bad_floppy_size_fails_cleanly() -> Result<()> {
        let path = temp_path("bad.adf");
        fs::write(&path, [0u8; 512])?;
        let err = parse_config(&format!(
            r#"
            [floppy.df0]
            path = "{}"
            "#,
            toml_path(&path)
        ))
        .unwrap_err();
        let _ = fs::remove_file(&path);
        assert!(err.to_string().contains("expected 901120 bytes"), "{err:#}");
        Ok(())
    }

    #[test]
    fn cli_overrides_select_a_machine_with_no_config_file() -> Result<()> {
        let overrides = ConfigOverrides {
            model: Some("A1200".to_string()),
            ..Default::default()
        };
        let cfg = Config::load_with_overrides(None, &overrides)?;
        assert_eq!(cfg.machine, Some(MachineModel::A1200));
        assert_eq!(cfg.cpu, CpuModel::M68EC020);
        assert_eq!(cfg.chipset, Chipset::Aga);
        assert_eq!(cfg.chip_ram_bytes, 2 * 1024 * 1024);
        Ok(())
    }

    #[test]
    fn cli_overrides_layer_on_top_of_a_profile() -> Result<()> {
        // A model plus explicit CPU/fast-RAM overrides: the profile supplies
        // the chipset and chip RAM, the overrides win where they are set, and
        // everything still goes through the normal validation/derivation.
        let overrides = ConfigOverrides {
            model: Some("A500".to_string()),
            cpu: Some("68020".to_string()),
            fpu: Some(true),
            cpu_clock_mhz: Some(28.0),
            fast: Some("4M".to_string()),
            ..Default::default()
        };
        let cfg = Config::load_with_overrides(None, &overrides)?;
        assert_eq!(cfg.machine, Some(MachineModel::A500));
        assert_eq!(cfg.cpu, CpuModel::M68020);
        assert!(cfg.fpu);
        assert_eq!(cfg.cpu_clock_mhz, 28.0);
        assert_eq!(cfg.fast_ram_bytes, 4 * 1024 * 1024);
        Ok(())
    }

    #[test]
    fn cli_overrides_are_validated_like_config_fields() {
        // A 68000 cannot carry an FPU; the override hits the same check as
        // `[cpu] fpu = true` would.
        let overrides = ConfigOverrides {
            cpu: Some("68000".to_string()),
            fpu: Some(true),
            ..Default::default()
        };
        let err = Config::load_with_overrides(None, &overrides).unwrap_err();
        assert!(err.to_string().contains("coprocessor interface"), "{err:#}");

        // An unknown chipset name is rejected by the shared parser.
        let overrides = ConfigOverrides {
            chipset: Some("OCS3".to_string()),
            ..Default::default()
        };
        let err = Config::load_with_overrides(None, &overrides).unwrap_err();
        assert!(err.to_string().contains("unknown chipset"), "{err:#}");
    }

    fn temp_adf() -> Result<PathBuf> {
        let path = temp_path("test.adf");
        fs::write(&path, vec![0u8; 80 * 2 * 11 * 512])?;
        Ok(path)
    }

    fn temp_path(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("copperline-config-test-{nanos}-{name}"))
    }

    fn toml_path(path: &Path) -> String {
        path.to_string_lossy()
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
    }
}
