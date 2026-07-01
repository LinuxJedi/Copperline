// SPDX-License-Identifier: GPL-3.0-or-later

//! The pre-boot machine-configuration screen's data model.
//!
//! When Copperline is started with no config (and from the "Machine
//! Configuration..." menu item) a launcher panel lets the user pick a machine
//! and configure everything about it before pressing Run. This module holds the
//! editable model behind that panel; the panel's pixel layout and hit-testing
//! live in [`crate::video::ui`], and the App integration (file dialogs, Run,
//! Save) lives in [`crate::video::window`].
//!
//! [`MachineSetup`] is a fully-typed editable mirror of the configurable
//! machine. It is built from, and converted back to, the loadable
//! [`RawConfig`]: loading parses a file into a `RawConfig`, validates it through
//! the existing `TryFrom<RawConfig> for Config` pipeline, then fills the typed
//! fields; Run and Save go the other way via [`MachineSetup::to_raw`], so the
//! configuration screen reuses all of the config layer's validation and
//! profile-default logic instead of duplicating it. `to_raw` emits only the
//! fields that differ from the selected profile's defaults, so a saved file
//! reads like the hand-written `*.example.toml`.

use crate::chipset::agnus::{AgnusRevision, VideoStandard};
use crate::chipset::denise::DeniseRevision;
use crate::config::{
    format_size, machine_profile_defaults, Chipset, Config, CpuModel, JoystickInputMode,
    MachineModel, Overscan, PacingBudget, PixelAspect, RawConfig, RawDrive, RawFloppyDrive,
    RawZorroBoard, WarpSpeed,
};
use crate::zorro::{ConfigOption, ConfigOptionKind, LoadedZorroBoard};
use anyhow::Result;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// A Zorro board entry in the launcher: its metadata-file path, the config
/// option schema parsed from that manifest (empty for RAM boards or on load
/// error), and the user's per-board setting overrides (layered over the
/// manifest defaults). Editing in the config panel mutates `overrides`.
#[derive(Debug, Clone)]
pub struct ZorroBoardSetup {
    metadata: PathBuf,
    options: Vec<ConfigOption>,
    /// Effective manifest defaults (option defaults overlaid by `[config]`,
    /// file paths resolved), the baseline the user's overrides layer over.
    defaults: BTreeMap<String, String>,
    overrides: BTreeMap<String, String>,
}

impl ZorroBoardSetup {
    /// Load a board's option schema + defaults from its manifest. RAM boards
    /// and load failures yield an entry with no editable options.
    fn load(metadata: PathBuf) -> Self {
        let (options, defaults) = match crate::zorro::load_board_metadata(&metadata) {
            Ok(LoadedZorroBoard::Wasm {
                options,
                default_config,
                ..
            }) => (options, default_config),
            _ => (Vec::new(), BTreeMap::new()),
        };
        Self {
            metadata,
            options,
            defaults,
            overrides: BTreeMap::new(),
        }
    }

    /// File name (or full path) for display.
    pub fn name(&self) -> String {
        self.metadata
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| self.metadata.display().to_string())
    }

    pub fn options(&self) -> &[ConfigOption] {
        &self.options
    }

    /// The current value of option `opt`: the user's override, else the
    /// effective manifest default, else empty.
    pub fn value(&self, opt: usize) -> String {
        let Some(o) = self.options.get(opt) else {
            return String::new();
        };
        self.overrides
            .get(&o.key)
            .or_else(|| self.defaults.get(&o.key))
            .cloned()
            .unwrap_or_default()
    }

    fn set(&mut self, opt: usize, value: String) {
        if let Some(o) = self.options.get(opt) {
            self.overrides.insert(o.key.clone(), value);
        }
    }

    /// Drop the override, reverting the option to its manifest default.
    fn clear(&mut self, opt: usize) {
        if let Some(o) = self.options.get(opt) {
            self.overrides.remove(&o.key);
        }
    }

    /// Step an enum/int option by one (forward or back).
    fn cycle(&mut self, opt: usize, forward: bool) {
        let Some(o) = self.options.get(opt) else {
            return;
        };
        let next = match &o.kind {
            ConfigOptionKind::Enum(choices) if !choices.is_empty() => {
                let cur = self.value(opt);
                let idx = choices.iter().position(|c| *c == cur).unwrap_or(0);
                let n = choices.len();
                let idx = if forward {
                    (idx + 1) % n
                } else {
                    (idx + n - 1) % n
                };
                choices[idx].clone()
            }
            ConfigOptionKind::Int => {
                let cur: i64 = self.value(opt).trim().parse().unwrap_or(0);
                let next = if forward { cur + 1 } else { cur - 1 };
                next.to_string()
            }
            _ => return,
        };
        self.set(opt, next);
    }

    /// Flip a bool option.
    fn toggle(&mut self, opt: usize) {
        if matches!(
            self.options.get(opt).map(|o| &o.kind),
            Some(ConfigOptionKind::Bool)
        ) {
            let on = self.value(opt).trim().eq_ignore_ascii_case("true");
            self.set(opt, (!on).to_string());
        }
    }

    /// The TOML override value for an option, typed per its kind, or `None`
    /// when the user has left it at the manifest default.
    fn override_toml(&self, o: &ConfigOption) -> Option<toml::Value> {
        let raw = self.overrides.get(&o.key)?;
        Some(match o.kind {
            ConfigOptionKind::Bool => toml::Value::Boolean(raw.trim().eq_ignore_ascii_case("true")),
            ConfigOptionKind::Int => raw
                .trim()
                .parse::<i64>()
                .map(toml::Value::Integer)
                .unwrap_or_else(|_| toml::Value::String(raw.clone())),
            _ => toml::Value::String(raw.clone()),
        })
    }
}

/// The configuration screen's category tabs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LauncherTab {
    System,
    Cpu,
    Memory,
    Rom,
    Floppy,
    Storage,
    Cd,
    Zorro,
    AvEmulation,
}

/// Tab strip, left to right.
pub const TABS: [LauncherTab; 9] = [
    LauncherTab::System,
    LauncherTab::Cpu,
    LauncherTab::Memory,
    LauncherTab::Rom,
    LauncherTab::Floppy,
    LauncherTab::Storage,
    LauncherTab::Cd,
    LauncherTab::Zorro,
    LauncherTab::AvEmulation,
];

impl LauncherTab {
    pub fn label(self) -> &'static str {
        match self {
            LauncherTab::System => "System",
            LauncherTab::Cpu => "CPU",
            LauncherTab::Memory => "Memory",
            LauncherTab::Rom => "ROM",
            LauncherTab::Floppy => "Floppy",
            LauncherTab::Storage => "Hard Disk",
            LauncherTab::Cd => "CD",
            LauncherTab::Zorro => "Zorro",
            LauncherTab::AvEmulation => "A/V & Emu",
        }
    }
}

/// A single editable setting. Parameter-free variants keep the per-tab row
/// tables and `UiControl` hit-testing simple (every control is one `Copy` enum
/// value); the floppy/SCSI families are spelled out rather than indexed for the
/// same reason.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LauncherField {
    // System
    Chipset,
    Agnus,
    Denise,
    Video,
    Rtc,
    Identify,
    // CPU
    Cpu,
    Fpu,
    Clock,
    Icache,
    Dcache,
    // Memory
    ChipRam,
    FastRam,
    SlowRam,
    Z3Ram,
    // ROM
    Rom,
    ExtendedRom,
    // Floppy
    FloppyDrives,
    Df0Image,
    Df0WriteProtect,
    Df1Image,
    Df1WriteProtect,
    Df2Image,
    Df2WriteProtect,
    Df3Image,
    Df3WriteProtect,
    // Hard disk
    IdeMaster,
    IdeSlave,
    ScsiRom,
    ScsiRomOdd,
    ScsiUnit0,
    ScsiUnit1,
    ScsiUnit2,
    ScsiUnit3,
    ScsiUnit4,
    ScsiUnit5,
    ScsiUnit6,
    // CD
    CdImage,
    CdInsertDelay,
    Cd32Nvram,
    // A/V and emulation
    Overscan,
    PixelAspect,
    Phosphor,
    FloppySounds,
    FloppyVolume,
    PowerOn,
    PacingBudget,
    RealtimePriority,
    Warp,
    Joystick,
}

/// How a row's value is edited, and therefore which widget the panel draws.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowKind {
    /// A `[<] value [>]` picker / stepper.
    Cycle,
    /// An On/Off button.
    Toggle,
    /// A file path with Browse/Clear buttons.
    Path,
    /// A hard-drive image: a path with Browse/Clear, plus an editable
    /// volume-name field (used when the image is a host directory).
    Drive,
}

/// One settings row: a label, the field it edits, and how to edit it.
#[derive(Debug, Clone, Copy)]
pub struct Row {
    pub field: LauncherField,
    pub label: &'static str,
    pub kind: RowKind,
}

const fn row(field: LauncherField, label: &'static str, kind: RowKind) -> Row {
    Row { field, label, kind }
}

use LauncherField as F;
use RowKind::{Cycle, Drive, Toggle};
// `RowKind::Path` is written out below so it does not collide with the
// `std::path::Path` import.
use RowKind::Path as PathRow;

const SYSTEM_ROWS: [Row; 6] = [
    row(F::Chipset, "Chipset", Cycle),
    row(F::Agnus, "Agnus", Cycle),
    row(F::Denise, "Denise", Cycle),
    row(F::Video, "Video", Cycle),
    row(F::Rtc, "Real-time clock", Toggle),
    row(F::Identify, "Identify board", Toggle),
];
const CPU_ROWS: [Row; 5] = [
    row(F::Cpu, "CPU", Cycle),
    row(F::Fpu, "FPU (68881/2)", Toggle),
    row(F::Clock, "Clock", Cycle),
    row(F::Icache, "Instruction cache", Toggle),
    row(F::Dcache, "Data cache", Toggle),
];
const MEMORY_ROWS: [Row; 4] = [
    row(F::ChipRam, "Chip RAM", Cycle),
    row(F::FastRam, "Fast RAM", Cycle),
    row(F::SlowRam, "Slow RAM", Cycle),
    row(F::Z3Ram, "Zorro III RAM", Cycle),
];
const ROM_ROWS: [Row; 2] = [
    row(F::Rom, "Kickstart ROM", PathRow),
    row(F::ExtendedRom, "Extended ROM", PathRow),
];
const FLOPPY_ROWS: [Row; 9] = [
    row(F::FloppyDrives, "Drives", Cycle),
    row(F::Df0Image, "DF0 image", PathRow),
    row(F::Df0WriteProtect, "DF0 write-protect", Toggle),
    row(F::Df1Image, "DF1 image", PathRow),
    row(F::Df1WriteProtect, "DF1 write-protect", Toggle),
    row(F::Df2Image, "DF2 image", PathRow),
    row(F::Df2WriteProtect, "DF2 write-protect", Toggle),
    row(F::Df3Image, "DF3 image", PathRow),
    row(F::Df3WriteProtect, "DF3 write-protect", Toggle),
];
const STORAGE_ROWS: [Row; 11] = [
    row(F::IdeMaster, "IDE master", Drive),
    row(F::IdeSlave, "IDE slave", Drive),
    row(F::ScsiRom, "SCSI boot ROM", PathRow),
    row(F::ScsiRomOdd, "SCSI ROM (odd)", PathRow),
    row(F::ScsiUnit0, "SCSI unit 0", Drive),
    row(F::ScsiUnit1, "SCSI unit 1", Drive),
    row(F::ScsiUnit2, "SCSI unit 2", Drive),
    row(F::ScsiUnit3, "SCSI unit 3", Drive),
    row(F::ScsiUnit4, "SCSI unit 4", Drive),
    row(F::ScsiUnit5, "SCSI unit 5", Drive),
    row(F::ScsiUnit6, "SCSI unit 6", Drive),
];
const CD_ROWS: [Row; 3] = [
    row(F::CdImage, "CD image", PathRow),
    row(F::CdInsertDelay, "Insert delay", Cycle),
    row(F::Cd32Nvram, "CD32 NVRAM", PathRow),
];
const AV_EMULATION_ROWS: [Row; 10] = [
    row(F::Overscan, "Overscan", Cycle),
    row(F::PixelAspect, "Pixel aspect", Cycle),
    row(F::Phosphor, "Phosphor", Cycle),
    row(F::FloppySounds, "Floppy sounds", Toggle),
    row(F::FloppyVolume, "Floppy volume", Cycle),
    row(F::PowerOn, "Power on at start", Toggle),
    row(F::PacingBudget, "Pacing budget", Cycle),
    row(F::RealtimePriority, "Realtime priority", Toggle),
    row(F::Warp, "Warp speed", Cycle),
    row(F::Joystick, "Joystick input", Cycle),
];

/// The rows shown on a tab, top to bottom. The `Zorro` tab has no rows: it is
/// drawn as a board list with Add/Remove controls (see the panel code).
pub fn rows(tab: LauncherTab) -> &'static [Row] {
    match tab {
        LauncherTab::System => &SYSTEM_ROWS,
        LauncherTab::Cpu => &CPU_ROWS,
        LauncherTab::Memory => &MEMORY_ROWS,
        LauncherTab::Rom => &ROM_ROWS,
        LauncherTab::Floppy => &FLOPPY_ROWS,
        LauncherTab::Storage => &STORAGE_ROWS,
        LauncherTab::Cd => &CD_ROWS,
        LauncherTab::Zorro => &[],
        LauncherTab::AvEmulation => &AV_EMULATION_ROWS,
    }
}

/// Machine models offered in the selector strip, roughly chronological.
pub const MODELS: [MachineModel; 8] = [
    MachineModel::A1000,
    MachineModel::A500Ocs,
    MachineModel::A500,
    MachineModel::A500Plus,
    MachineModel::A600,
    MachineModel::A1200,
    MachineModel::Cdtv,
    MachineModel::Cd32,
];

// --- value preset lists for the cycle/stepper controls -------------------

const CHIPSETS: [Chipset; 3] = [Chipset::Ocs, Chipset::Ecs, Chipset::Aga];
const AGNUS_CHOICES: [Option<AgnusRevision>; 5] = [
    None,
    Some(AgnusRevision::Ocs),
    Some(AgnusRevision::Ecs8372Rev4),
    Some(AgnusRevision::Ecs8375),
    Some(AgnusRevision::AgaAlice),
];
const DENISE_CHOICES: [Option<DeniseRevision>; 4] = [
    None,
    Some(DeniseRevision::Ocs),
    Some(DeniseRevision::Ecs8373),
    Some(DeniseRevision::AgaLisa),
];
const VIDEO_CHOICES: [VideoStandard; 2] = [VideoStandard::Pal, VideoStandard::Ntsc];
const CPUS: [CpuModel; 5] = [
    CpuModel::M68000,
    CpuModel::M68EC020,
    CpuModel::M68020,
    CpuModel::M68030,
    CpuModel::M68040,
];
const CLOCK_PRESETS: [f64; 8] = [7.09, 14.0, 14.18, 25.0, 28.0, 33.0, 40.0, 50.0];
const CHIP_PRESETS: [usize; 4] = [256 * 1024, 512 * 1024, 1024 * 1024, 2 * 1024 * 1024];
const FAST_PRESETS: [usize; 9] = [
    0,
    64 * 1024,
    128 * 1024,
    256 * 1024,
    512 * 1024,
    1024 * 1024,
    2 * 1024 * 1024,
    4 * 1024 * 1024,
    8 * 1024 * 1024,
];
const SLOW_PRESETS: [usize; 3] = [0, 256 * 1024, 512 * 1024];
const Z3_PRESETS: [usize; 8] = [
    0,
    16 * 1024 * 1024,
    32 * 1024 * 1024,
    64 * 1024 * 1024,
    128 * 1024 * 1024,
    256 * 1024 * 1024,
    512 * 1024 * 1024,
    1024 * 1024 * 1024,
];
const OVERSCANS: [Overscan; 2] = [Overscan::Tv, Overscan::Full];
const PIXEL_ASPECTS: [PixelAspect; 2] = [PixelAspect::Tv, PixelAspect::Square];
const PACINGS: [PacingBudget; 2] = [PacingBudget::Cycles, PacingBudget::Instructions];
const WARPS: [WarpSpeed; 5] = [
    WarpSpeed::X2,
    WarpSpeed::X4,
    WarpSpeed::X8,
    WarpSpeed::X16,
    WarpSpeed::Max,
];
// The stepper flips the two explicit modes, matching the runtime toggle.
const JOYSTICK_MODES: [JoystickInputMode; 2] =
    [JoystickInputMode::Gamepad, JoystickInputMode::Keyboard];

/// A fully-typed, editable mirror of a configurable machine. See the module
/// docs for how it round-trips through [`RawConfig`].
#[derive(Debug, Clone)]
pub struct MachineSetup {
    /// Selected machine profile (`None` is the no-profile default, equivalent
    /// to the `A500`).
    model: Option<MachineModel>,
    // System
    chipset: Chipset,
    /// Explicit Agnus override; `None` derives from the chipset/profile.
    agnus: Option<AgnusRevision>,
    /// Explicit Denise override; `None` derives from the chipset/profile.
    denise: Option<DeniseRevision>,
    video: VideoStandard,
    rtc: bool,
    identify: bool,
    // CPU
    cpu: CpuModel,
    fpu: bool,
    clock_mhz: f64,
    icache: bool,
    dcache: bool,
    // Memory (bytes)
    chip_ram: usize,
    fast_ram: usize,
    slow_ram: usize,
    z3_ram: usize,
    // ROM (None = bundled AROS for the boot ROM, none for extended)
    rom: Option<PathBuf>,
    extended_rom: Option<PathBuf>,
    // Floppy
    floppy_drives: u8,
    /// Per-drive disk-swap playlists (entry 0 is the boot disk). A single
    /// image is a one-element list.
    df_playlists: [Vec<PathBuf>; 4],
    df_write_protected: [bool; 4],
    // Hard disk. Each drive's optional volume-name override (directory mounts
    // only) sits in the matching `*_name` slot, paralleling the path slot.
    ide_master: Option<PathBuf>,
    ide_master_name: Option<String>,
    ide_slave: Option<PathBuf>,
    ide_slave_name: Option<String>,
    scsi_rom: Option<PathBuf>,
    scsi_rom_odd: Option<PathBuf>,
    scsi_units: [Option<PathBuf>; 7],
    scsi_unit_names: [Option<String>; 7],
    // CD
    cd_image: Option<PathBuf>,
    cd_insert_delay: f64,
    cd32_nvram: Option<PathBuf>,
    // A/V and emulation
    overscan: Overscan,
    pixel_aspect: PixelAspect,
    phosphor: f32,
    floppy_sounds: bool,
    floppy_volume: u8,
    power_on: bool,
    pacing_budget: PacingBudget,
    realtime_priority: bool,
    warp: WarpSpeed,
    joystick_input_mode: JoystickInputMode,
    // Extra Zorro boards (metadata path + plugin config schema/overrides)
    zorro_boards: Vec<ZorroBoardSetup>,
}

impl Default for MachineSetup {
    fn default() -> Self {
        // The empty raw config is always valid (the built-in defaults).
        Self::from_raw(&RawConfig::default()).expect("default config is valid")
    }
}

impl MachineSetup {
    /// Build the typed model from a raw config, validating it through the
    /// config pipeline first. The validated [`Config`] supplies the resolved
    /// scalar values; the raw view supplies the things `Config` does not
    /// preserve: whether the Agnus/Denise were explicit overrides, the
    /// "no boot ROM = AROS" distinction, and the `[[zorro]]` board paths.
    pub fn from_raw(raw: &RawConfig) -> Result<Self> {
        let cfg: Config = raw.clone().try_into()?;
        let df_write_protected = std::array::from_fn(|i| {
            cfg.floppy.drives[i]
                .as_ref()
                .map(|d| d.write_protected)
                .unwrap_or(true)
        });
        let connected = cfg.floppy_connected.iter().filter(|&&c| c).count().max(1) as u8;
        Ok(Self {
            model: cfg.machine,
            chipset: cfg.chipset,
            agnus: raw.chipset.agnus.is_some().then_some(cfg.agnus_revision),
            denise: raw.chipset.denise.is_some().then_some(cfg.denise_revision),
            video: cfg.video_standard,
            rtc: cfg.rtc_present,
            identify: cfg.identify_board,
            cpu: cfg.cpu,
            fpu: cfg.fpu,
            clock_mhz: cfg.cpu_clock_mhz,
            icache: cfg.cpu_icache,
            dcache: cfg.cpu_dcache,
            chip_ram: cfg.chip_ram_bytes,
            fast_ram: cfg.fast_ram_bytes,
            slow_ram: cfg.slow_ram_bytes,
            z3_ram: cfg.z3_ram_bytes,
            rom: raw.rom.as_deref().map(PathBuf::from),
            extended_rom: raw.extended_rom.as_deref().map(PathBuf::from),
            floppy_drives: raw.floppy.drives.unwrap_or(connected).clamp(1, 4),
            df_playlists: cfg.floppy_playlists.clone(),
            df_write_protected,
            ide_master: cfg.ide.master.as_ref().map(|d| d.path.clone()),
            ide_master_name: cfg.ide.master.as_ref().and_then(|d| d.volume_name.clone()),
            ide_slave: cfg.ide.slave.as_ref().map(|d| d.path.clone()),
            ide_slave_name: cfg.ide.slave.as_ref().and_then(|d| d.volume_name.clone()),
            scsi_rom: cfg.scsi.rom.clone(),
            scsi_rom_odd: cfg.scsi.rom_odd.clone(),
            scsi_units: std::array::from_fn(|i| cfg.scsi.units[i].as_ref().map(|d| d.path.clone())),
            scsi_unit_names: std::array::from_fn(|i| {
                cfg.scsi.units[i]
                    .as_ref()
                    .and_then(|d| d.volume_name.clone())
            }),
            cd_image: cfg.cd_image_path.clone(),
            cd_insert_delay: cfg.cd_insert_delay_secs,
            // Use the raw NVRAM path: Config defaults it to "cd32-nvram.bin"
            // on CD32, which we do not want to persist as an explicit setting.
            cd32_nvram: raw.cd.nvram.as_deref().map(PathBuf::from),
            overscan: cfg.overscan,
            pixel_aspect: cfg.pixel_aspect,
            phosphor: cfg.phosphor,
            floppy_sounds: cfg.audio.floppy_sounds,
            floppy_volume: cfg.audio.floppy_sounds_volume,
            power_on: cfg.emulation.power_on,
            pacing_budget: cfg.emulation.pacing_budget,
            realtime_priority: cfg.emulation.realtime_priority,
            warp: cfg.emulation.warp_speed,
            joystick_input_mode: cfg.joystick_input_mode,
            zorro_boards: raw
                .zorro
                .iter()
                .map(|b| {
                    let mut board = ZorroBoardSetup::load(PathBuf::from(&b.metadata));
                    if let Some(overrides) = &b.config {
                        for (key, value) in overrides {
                            board
                                .overrides
                                .insert(key.clone(), crate::zorro::toml_value_to_string(value));
                        }
                    }
                    board
                })
                .collect(),
        })
    }

    /// Load a configuration file into the typed model, validating it.
    pub fn load_from(path: &Path) -> Result<Self> {
        Self::from_raw(&crate::config::raw_from_path(path)?)
    }

    /// The bare-profile config this setup is compared against when emitting
    /// minimal TOML: the machine the selected profile produces with no
    /// overrides, resolved through the same `TryFrom` as a real boot so the
    /// comparison matches exactly (including derived clock/cache defaults).
    fn baseline(&self) -> Config {
        let mut raw = RawConfig::default();
        raw.machine.profile = self.model.map(|m| model_name(m).to_string());
        raw.try_into().unwrap_or_else(|_| {
            self.model
                .map_or_else(Config::default, machine_profile_defaults)
        })
    }

    /// Convert back to a raw config, emitting only the fields that differ from
    /// the selected profile's defaults (so saved files stay minimal).
    pub fn to_raw(&self) -> RawConfig {
        let base = self.baseline();
        let mut raw = RawConfig::default();
        if let Some(m) = self.model {
            raw.machine.profile = Some(model_name(m).to_string());
        }
        // System
        if self.chipset != base.chipset {
            raw.chipset.revision = Some(chipset_name(self.chipset).to_string());
        }
        if let Some(a) = self.agnus {
            raw.chipset.agnus = Some(agnus_name(a).to_string());
        }
        if let Some(d) = self.denise {
            raw.chipset.denise = Some(denise_name(d).to_string());
        }
        if self.video != base.video_standard {
            raw.chipset.video = Some(video_name(self.video).to_string());
        }
        if self.rtc != base.rtc_present {
            raw.machine.rtc = Some(self.rtc);
        }
        if self.identify != base.identify_board {
            raw.identify = Some(self.identify);
        }
        // CPU
        if self.cpu != base.cpu {
            raw.cpu.model = Some(cpu_name(self.cpu).to_string());
        }
        if self.fpu != base.fpu {
            raw.cpu.fpu = Some(self.fpu);
        }
        if (self.clock_mhz - base.cpu_clock_mhz).abs() > 1e-9 {
            raw.cpu.clock_mhz = Some(self.clock_mhz);
        }
        if self.icache != base.cpu_icache {
            raw.cpu.icache = Some(self.icache);
        }
        if self.dcache != base.cpu_dcache {
            raw.cpu.dcache = Some(self.dcache);
        }
        // Memory
        if self.chip_ram != base.chip_ram_bytes {
            raw.memory.chip = Some(format_size(self.chip_ram));
        }
        if self.fast_ram != base.fast_ram_bytes {
            raw.memory.fast = Some(format_size(self.fast_ram));
        }
        if self.slow_ram != base.slow_ram_bytes {
            raw.memory.slow = Some(format_size(self.slow_ram));
        }
        if self.z3_ram != base.z3_ram_bytes {
            raw.memory.z3 = Some(format_size(self.z3_ram));
        }
        // ROM
        raw.rom = self.rom.as_deref().map(path_string);
        raw.extended_rom = self.extended_rom.as_deref().map(path_string);
        // Floppy: cover any drive carrying media so the count never orphans it.
        let media_max = self
            .df_playlists
            .iter()
            .rposition(|p| !p.is_empty())
            .map(|i| i as u8 + 1)
            .unwrap_or(1);
        let drives = self.floppy_drives.max(media_max);
        if drives != 1 {
            raw.floppy.drives = Some(drives);
        }
        raw.floppy.df0 = self.floppy_drive_raw(0);
        raw.floppy.df1 = self.floppy_drive_raw(1);
        raw.floppy.df2 = self.floppy_drive_raw(2);
        raw.floppy.df3 = self.floppy_drive_raw(3);
        // Hard disk
        raw.ide.master = drive_raw(self.ide_master.as_deref(), self.ide_master_name.as_deref());
        raw.ide.slave = drive_raw(self.ide_slave.as_deref(), self.ide_slave_name.as_deref());
        raw.scsi.rom = self.scsi_rom.as_deref().map(path_string);
        raw.scsi.rom_odd = self.scsi_rom_odd.as_deref().map(path_string);
        raw.scsi.unit0 = drive_raw(
            self.scsi_units[0].as_deref(),
            self.scsi_unit_names[0].as_deref(),
        );
        raw.scsi.unit1 = drive_raw(
            self.scsi_units[1].as_deref(),
            self.scsi_unit_names[1].as_deref(),
        );
        raw.scsi.unit2 = drive_raw(
            self.scsi_units[2].as_deref(),
            self.scsi_unit_names[2].as_deref(),
        );
        raw.scsi.unit3 = drive_raw(
            self.scsi_units[3].as_deref(),
            self.scsi_unit_names[3].as_deref(),
        );
        raw.scsi.unit4 = drive_raw(
            self.scsi_units[4].as_deref(),
            self.scsi_unit_names[4].as_deref(),
        );
        raw.scsi.unit5 = drive_raw(
            self.scsi_units[5].as_deref(),
            self.scsi_unit_names[5].as_deref(),
        );
        raw.scsi.unit6 = drive_raw(
            self.scsi_units[6].as_deref(),
            self.scsi_unit_names[6].as_deref(),
        );
        // CD
        raw.cd.image = self.cd_image.as_deref().map(path_string);
        if self.cd_insert_delay != 0.0 {
            raw.cd.insert_delay = Some(self.cd_insert_delay);
        }
        raw.cd.nvram = self.cd32_nvram.as_deref().map(path_string);
        // A/V and emulation
        if self.overscan != base.overscan {
            raw.display.overscan = Some(overscan_name(self.overscan).to_string());
        }
        if self.pixel_aspect != base.pixel_aspect {
            raw.display.pixel_aspect = Some(pixel_aspect_name(self.pixel_aspect).to_string());
        }
        if (self.phosphor - base.phosphor).abs() > 1e-6 {
            raw.display.phosphor = Some(self.phosphor);
        }
        if self.floppy_sounds != base.audio.floppy_sounds {
            raw.audio.floppy_sounds = Some(self.floppy_sounds);
        }
        if self.floppy_volume != base.audio.floppy_sounds_volume {
            raw.audio.floppy_sounds_volume = Some(u16::from(self.floppy_volume));
        }
        if self.power_on != base.emulation.power_on {
            raw.emulation.power_on = Some(self.power_on);
        }
        if self.pacing_budget != base.emulation.pacing_budget {
            raw.emulation.pacing_budget = Some(pacing_name(self.pacing_budget).to_string());
        }
        if self.realtime_priority != base.emulation.realtime_priority {
            raw.emulation.realtime_priority = Some(self.realtime_priority);
        }
        if self.warp != base.emulation.warp_speed {
            raw.emulation.warp_speed = Some(self.warp.label().to_ascii_lowercase());
        }
        if self.joystick_input_mode != base.joystick_input_mode {
            raw.input.joystick = Some(self.joystick_input_mode.label().to_string());
        }
        // Zorro boards: emit the metadata path plus any per-board overrides
        // (typed per the option schema), only when the user changed something.
        raw.zorro = self
            .zorro_boards
            .iter()
            .map(|b| {
                let mut table = toml::Table::new();
                for o in &b.options {
                    if let Some(v) = b.override_toml(o) {
                        table.insert(o.key.clone(), v);
                    }
                }
                RawZorroBoard {
                    metadata: path_string(&b.metadata),
                    config: (!table.is_empty()).then_some(table),
                }
            })
            .collect();
        raw
    }

    fn floppy_drive_raw(&self, idx: usize) -> Option<RawFloppyDrive> {
        let playlist = &self.df_playlists[idx];
        if playlist.is_empty() {
            // A write-protect flag on an empty drive is meaningless, so an
            // untouched/empty drive emits no [floppy.dfN] table at all.
            return None;
        }
        let (first, rest) = playlist.split_first().expect("non-empty checked above");
        Some(RawFloppyDrive {
            enabled: None,
            path: Some(path_string(first)),
            paths: (!rest.is_empty()).then(|| rest.iter().map(|p| path_string(p)).collect()),
            // write_protected defaults to true; only an unprotected drive is
            // written explicitly.
            write_protected: (!self.df_write_protected[idx]).then_some(false),
        })
    }

    /// Serialize the configured machine to TOML for the Save action.
    pub fn to_toml(&self) -> Result<String> {
        self.to_raw().to_toml_string()
    }

    /// Validate the configured machine, producing the [`Config`] the Run action
    /// builds from (its boot ROM may still be the AROS sentinel; the caller
    /// resolves that).
    pub fn build_config(&self) -> Result<Config> {
        self.to_raw().try_into()
    }

    pub fn model(&self) -> Option<MachineModel> {
        self.model
    }

    /// The model to show as selected in the picker. With no profile chosen the
    /// machine equals the A500 defaults, so the A500 button is highlighted.
    pub fn selected_model(&self) -> MachineModel {
        self.model.unwrap_or(MachineModel::A500)
    }

    /// Switch machine profile, resetting the profile-derived fields to the new
    /// model's defaults and dropping media the new model cannot use (so a
    /// later Run does not fail validation on a stale IDE/CD image). Boot media
    /// the model can still carry (ROM, floppies, SCSI, Zorro) is kept.
    pub fn select_model(&mut self, model: Option<MachineModel>) {
        self.model = model;
        let base = self.baseline();
        self.chipset = base.chipset;
        self.agnus = None;
        self.denise = None;
        self.video = base.video_standard;
        self.rtc = base.rtc_present;
        self.identify = base.identify_board;
        self.cpu = base.cpu;
        self.fpu = base.fpu;
        self.clock_mhz = base.cpu_clock_mhz;
        self.icache = base.cpu_icache;
        self.dcache = base.cpu_dcache;
        self.chip_ram = base.chip_ram_bytes;
        self.fast_ram = base.fast_ram_bytes;
        self.slow_ram = base.slow_ram_bytes;
        self.z3_ram = base.z3_ram_bytes;
        self.overscan = base.overscan;
        self.pixel_aspect = base.pixel_aspect;
        self.phosphor = base.phosphor;
        self.floppy_sounds = base.audio.floppy_sounds;
        self.floppy_volume = base.audio.floppy_sounds_volume;
        self.power_on = base.emulation.power_on;
        self.pacing_budget = base.emulation.pacing_budget;
        self.realtime_priority = base.emulation.realtime_priority;
        self.warp = base.emulation.warp_speed;
        self.joystick_input_mode = base.joystick_input_mode;
        if !self.has_gayle() {
            self.ide_master = None;
            self.ide_master_name = None;
            self.ide_slave = None;
            self.ide_slave_name = None;
        }
        if !self.has_cd() {
            self.cd_image = None;
            self.cd_insert_delay = 0.0;
        }
        if model != Some(MachineModel::Cd32) {
            self.cd32_nvram = None;
        }
    }

    fn has_gayle(&self) -> bool {
        matches!(self.model, Some(MachineModel::A600 | MachineModel::A1200))
    }

    fn has_cd(&self) -> bool {
        matches!(self.model, Some(MachineModel::Cdtv | MachineModel::Cd32))
    }

    /// Whether a field is applicable to the current machine (greyed otherwise).
    pub fn applies(&self, field: LauncherField) -> bool {
        self.disabled_reason(field).is_none()
    }

    /// Why a field is greyed out for the current machine, shown in place of its
    /// controls so the constraint is explained rather than just disabled.
    /// `None` means the field is editable.
    pub fn disabled_reason(&self, field: LauncherField) -> Option<&'static str> {
        // `reason` is returned when the applicability condition is *false*.
        let reason = |applicable: bool, why: &'static str| (!applicable).then_some(why);
        match field {
            F::Fpu => reason(self.cpu != CpuModel::M68000, "needs 68020+"),
            // Gate on the model's actual cache capability so the launcher tracks
            // CpuModel rather than a second hand-maintained list (the 040 has
            // both caches; only the 68000 has neither).
            F::Icache => reason(self.cpu.has_instruction_cache(), "needs 68020+"),
            F::Dcache => reason(self.cpu.has_data_cache(), "needs 68030/040"),
            F::Z3Ram => reason(cpu_is_32bit(self.cpu), "needs 32-bit CPU"),
            F::IdeMaster | F::IdeSlave => reason(self.has_gayle(), "needs A600/A1200"),
            F::CdImage | F::CdInsertDelay => reason(self.has_cd(), "needs CDTV/CD32"),
            F::Cd32Nvram => reason(self.model == Some(MachineModel::Cd32), "CD32 only"),
            F::Df0Image | F::Df0WriteProtect => reason(self.floppy_drives >= 1, "drive off"),
            F::Df1Image | F::Df1WriteProtect => reason(self.floppy_drives >= 2, "drive off"),
            F::Df2Image | F::Df2WriteProtect => reason(self.floppy_drives >= 3, "drive off"),
            F::Df3Image | F::Df3WriteProtect => reason(self.floppy_drives >= 4, "drive off"),
            _ => None,
        }
    }

    /// The current boolean of a toggle field.
    pub fn toggle_value(&self, field: LauncherField) -> bool {
        match field {
            F::Rtc => self.rtc,
            F::Identify => self.identify,
            F::Fpu => self.fpu,
            F::Icache => self.icache,
            F::Dcache => self.dcache,
            F::Df0WriteProtect => self.df_write_protected[0],
            F::Df1WriteProtect => self.df_write_protected[1],
            F::Df2WriteProtect => self.df_write_protected[2],
            F::Df3WriteProtect => self.df_write_protected[3],
            F::FloppySounds => self.floppy_sounds,
            F::PowerOn => self.power_on,
            F::RealtimePriority => self.realtime_priority,
            _ => false,
        }
    }

    /// The current path of a path field, if any.
    pub fn path(&self, field: LauncherField) -> Option<&Path> {
        match field {
            F::Rom => self.rom.as_deref(),
            F::ExtendedRom => self.extended_rom.as_deref(),
            F::Df0Image => self.df_playlists[0].first().map(PathBuf::as_path),
            F::Df1Image => self.df_playlists[1].first().map(PathBuf::as_path),
            F::Df2Image => self.df_playlists[2].first().map(PathBuf::as_path),
            F::Df3Image => self.df_playlists[3].first().map(PathBuf::as_path),
            F::IdeMaster => self.ide_master.as_deref(),
            F::IdeSlave => self.ide_slave.as_deref(),
            F::ScsiRom => self.scsi_rom.as_deref(),
            F::ScsiRomOdd => self.scsi_rom_odd.as_deref(),
            F::ScsiUnit0 => self.scsi_units[0].as_deref(),
            F::ScsiUnit1 => self.scsi_units[1].as_deref(),
            F::ScsiUnit2 => self.scsi_units[2].as_deref(),
            F::ScsiUnit3 => self.scsi_units[3].as_deref(),
            F::ScsiUnit4 => self.scsi_units[4].as_deref(),
            F::ScsiUnit5 => self.scsi_units[5].as_deref(),
            F::ScsiUnit6 => self.scsi_units[6].as_deref(),
            F::CdImage => self.cd_image.as_deref(),
            F::Cd32Nvram => self.cd32_nvram.as_deref(),
            _ => None,
        }
    }

    /// Whether `field` is a hard-drive image that can carry a volume-name
    /// override (IDE/SCSI drives, but not the SCSI boot ROM or CD/ROM paths).
    pub fn is_drive_field(field: LauncherField) -> bool {
        matches!(
            field,
            F::IdeMaster
                | F::IdeSlave
                | F::ScsiUnit0
                | F::ScsiUnit1
                | F::ScsiUnit2
                | F::ScsiUnit3
                | F::ScsiUnit4
                | F::ScsiUnit5
                | F::ScsiUnit6
        )
    }

    /// The volume-name override for a drive field, if set.
    pub fn drive_name(&self, field: LauncherField) -> Option<&str> {
        let name = match field {
            F::IdeMaster => &self.ide_master_name,
            F::IdeSlave => &self.ide_slave_name,
            F::ScsiUnit0 => &self.scsi_unit_names[0],
            F::ScsiUnit1 => &self.scsi_unit_names[1],
            F::ScsiUnit2 => &self.scsi_unit_names[2],
            F::ScsiUnit3 => &self.scsi_unit_names[3],
            F::ScsiUnit4 => &self.scsi_unit_names[4],
            F::ScsiUnit5 => &self.scsi_unit_names[5],
            F::ScsiUnit6 => &self.scsi_unit_names[6],
            _ => return None,
        };
        name.as_deref()
    }

    /// Set (or, with a blank string, clear) a drive field's volume-name
    /// override. A name without a configured image is meaningless, so it is
    /// dropped when the field has no path.
    pub fn set_drive_name(&mut self, field: LauncherField, name: String) {
        let trimmed = name.trim();
        let value =
            (!trimmed.is_empty() && self.path(field).is_some()).then(|| trimmed.to_string());
        let slot = match field {
            F::IdeMaster => &mut self.ide_master_name,
            F::IdeSlave => &mut self.ide_slave_name,
            F::ScsiUnit0 => &mut self.scsi_unit_names[0],
            F::ScsiUnit1 => &mut self.scsi_unit_names[1],
            F::ScsiUnit2 => &mut self.scsi_unit_names[2],
            F::ScsiUnit3 => &mut self.scsi_unit_names[3],
            F::ScsiUnit4 => &mut self.scsi_unit_names[4],
            F::ScsiUnit5 => &mut self.scsi_unit_names[5],
            F::ScsiUnit6 => &mut self.scsi_unit_names[6],
            _ => return,
        };
        *slot = value;
    }

    /// The value text shown on a row (the current enum/size/number; the file
    /// name or a placeholder for paths; On/Off for toggles).
    pub fn value_label(&self, field: LauncherField) -> String {
        match field {
            F::Chipset => chipset_name(self.chipset).to_string(),
            F::Agnus => match self.agnus {
                None => "Auto".to_string(),
                Some(a) => agnus_name(a).to_string(),
            },
            F::Denise => match self.denise {
                None => "Auto".to_string(),
                Some(d) => denise_name(d).to_string(),
            },
            F::Video => video_name(self.video).to_string(),
            F::Cpu => cpu_name(self.cpu).to_string(),
            F::Clock => format_mhz(self.clock_mhz),
            F::ChipRam => size_label(self.chip_ram),
            F::FastRam => size_label(self.fast_ram),
            F::SlowRam => size_label(self.slow_ram),
            F::Z3Ram => size_label(self.z3_ram),
            F::FloppyDrives => self.floppy_drives.to_string(),
            F::CdInsertDelay => {
                if self.cd_insert_delay <= 0.0 {
                    "At boot".to_string()
                } else {
                    format!("{:.0} s", self.cd_insert_delay)
                }
            }
            F::Overscan => match self.overscan {
                Overscan::Tv => "TV".to_string(),
                Overscan::Full => "Full".to_string(),
            },
            F::PixelAspect => match self.pixel_aspect {
                PixelAspect::Tv => "TV (4:3)".to_string(),
                PixelAspect::Square => "Square".to_string(),
            },
            F::Phosphor => {
                if self.phosphor <= 0.0 {
                    "Off".to_string()
                } else {
                    format!("{:.2}", self.phosphor)
                }
            }
            F::FloppyVolume => format!("{}%", self.floppy_volume),
            F::PacingBudget => match self.pacing_budget {
                PacingBudget::Cycles => "Cycles".to_string(),
                PacingBudget::Instructions => "Instructions".to_string(),
            },
            F::Warp => self.warp.label().to_string(),
            F::Joystick => match self.joystick_input_mode {
                JoystickInputMode::Keyboard => "Keyboard".to_string(),
                JoystickInputMode::Gamepad => "Gamepad".to_string(),
            },
            // Path/drive fields: the file name, or a placeholder.
            F::Rom => self.path_label(field, "(bundled AROS)"),
            _ if rows_contains_kind(field, RowKind::Path)
                || rows_contains_kind(field, RowKind::Drive) =>
            {
                self.path_label(field, "(none)")
            }
            // Toggles
            _ => {
                if self.toggle_value(field) {
                    "On".to_string()
                } else {
                    "Off".to_string()
                }
            }
        }
    }

    fn path_label(&self, field: LauncherField, empty: &str) -> String {
        match self.path(field) {
            Some(p) => p
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| p.display().to_string()),
            None => empty.to_string(),
        }
    }

    /// Step a cycle/stepper field forward (`forward`) or backward.
    pub fn cycle(&mut self, field: LauncherField, forward: bool) {
        match field {
            F::Chipset => self.chipset = cycle_slice(&CHIPSETS, self.chipset, forward),
            F::Agnus => self.agnus = cycle_slice(&AGNUS_CHOICES, self.agnus, forward),
            F::Denise => self.denise = cycle_slice(&DENISE_CHOICES, self.denise, forward),
            F::Video => self.video = cycle_slice(&VIDEO_CHOICES, self.video, forward),
            F::Cpu => {
                self.cpu = cycle_slice(&CPUS, self.cpu, forward);
                // Re-derive the CPU-dependent toggles for the new part, as if
                // the model had been picked fresh (the panel greys whichever
                // do not apply).
                self.fpu = self.cpu.default_fpu();
                self.icache = self.cpu.has_instruction_cache();
                self.dcache = self.cpu.has_data_cache();
                self.clock_mhz = self.cpu.default_clock_mhz();
                if !cpu_is_32bit(self.cpu) {
                    self.z3_ram = 0;
                }
            }
            F::Clock => self.clock_mhz = cycle_floats(&CLOCK_PRESETS, self.clock_mhz, forward),
            F::ChipRam => self.chip_ram = cycle_slice(&CHIP_PRESETS, self.chip_ram, forward),
            F::FastRam => self.fast_ram = cycle_nearest(&FAST_PRESETS, self.fast_ram, forward),
            F::SlowRam => self.slow_ram = cycle_nearest(&SLOW_PRESETS, self.slow_ram, forward),
            F::Z3Ram => self.z3_ram = cycle_nearest(&Z3_PRESETS, self.z3_ram, forward),
            F::FloppyDrives => {
                self.floppy_drives = step_u8(self.floppy_drives, forward, 1, 4);
            }
            F::CdInsertDelay => {
                let secs = self.cd_insert_delay + if forward { 1.0 } else { -1.0 };
                self.cd_insert_delay = secs.clamp(0.0, 60.0);
            }
            F::Phosphor => {
                let p = self.phosphor + if forward { 0.05 } else { -0.05 };
                // Snap to the 0.05 grid to avoid float drift accumulating.
                self.phosphor = (p.clamp(0.0, 0.95) * 20.0).round() / 20.0;
            }
            F::FloppyVolume => self.floppy_volume = step_u8(self.floppy_volume, forward, 0, 100),
            F::Overscan => self.overscan = cycle_slice(&OVERSCANS, self.overscan, forward),
            F::PixelAspect => {
                self.pixel_aspect = cycle_slice(&PIXEL_ASPECTS, self.pixel_aspect, forward)
            }
            F::PacingBudget => {
                self.pacing_budget = cycle_slice(&PACINGS, self.pacing_budget, forward)
            }
            F::Warp => self.warp = cycle_slice(&WARPS, self.warp, forward),
            F::Joystick => {
                self.joystick_input_mode =
                    cycle_slice(&JOYSTICK_MODES, self.joystick_input_mode, forward)
            }
            _ => {}
        }
    }

    /// Flip a toggle field (no-op if the field is not a toggle).
    pub fn toggle(&mut self, field: LauncherField) {
        match field {
            F::Rtc => self.rtc = !self.rtc,
            F::Identify => self.identify = !self.identify,
            F::Fpu => self.fpu = !self.fpu,
            F::Icache => self.icache = !self.icache,
            F::Dcache => self.dcache = !self.dcache,
            F::Df0WriteProtect => self.df_write_protected[0] = !self.df_write_protected[0],
            F::Df1WriteProtect => self.df_write_protected[1] = !self.df_write_protected[1],
            F::Df2WriteProtect => self.df_write_protected[2] = !self.df_write_protected[2],
            F::Df3WriteProtect => self.df_write_protected[3] = !self.df_write_protected[3],
            F::FloppySounds => self.floppy_sounds = !self.floppy_sounds,
            F::PowerOn => self.power_on = !self.power_on,
            F::RealtimePriority => self.realtime_priority = !self.realtime_priority,
            _ => {}
        }
    }

    /// Set a path field's value (a floppy image replaces that drive's
    /// playlist with a single disk and wires the drive in).
    pub fn set_path(&mut self, field: LauncherField, path: PathBuf) {
        match field {
            F::Rom => self.rom = Some(path),
            F::ExtendedRom => self.extended_rom = Some(path),
            F::Df0Image => self.set_floppy(0, path),
            F::Df1Image => self.set_floppy(1, path),
            F::Df2Image => self.set_floppy(2, path),
            F::Df3Image => self.set_floppy(3, path),
            F::IdeMaster => self.ide_master = Some(path),
            F::IdeSlave => self.ide_slave = Some(path),
            F::ScsiRom => self.scsi_rom = Some(path),
            F::ScsiRomOdd => self.scsi_rom_odd = Some(path),
            F::ScsiUnit0 => self.scsi_units[0] = Some(path),
            F::ScsiUnit1 => self.scsi_units[1] = Some(path),
            F::ScsiUnit2 => self.scsi_units[2] = Some(path),
            F::ScsiUnit3 => self.scsi_units[3] = Some(path),
            F::ScsiUnit4 => self.scsi_units[4] = Some(path),
            F::ScsiUnit5 => self.scsi_units[5] = Some(path),
            F::ScsiUnit6 => self.scsi_units[6] = Some(path),
            F::CdImage => self.cd_image = Some(path),
            F::Cd32Nvram => self.cd32_nvram = Some(path),
            _ => {}
        }
    }

    fn set_floppy(&mut self, idx: usize, path: PathBuf) {
        self.df_playlists[idx] = vec![path];
        // Wire the drive in if it was beyond the configured count.
        self.floppy_drives = self.floppy_drives.max(idx as u8 + 1);
    }

    /// Clear a path field's value.
    pub fn clear_path(&mut self, field: LauncherField) {
        match field {
            F::Rom => self.rom = None,
            F::ExtendedRom => self.extended_rom = None,
            F::Df0Image => self.df_playlists[0].clear(),
            F::Df1Image => self.df_playlists[1].clear(),
            F::Df2Image => self.df_playlists[2].clear(),
            F::Df3Image => self.df_playlists[3].clear(),
            F::IdeMaster => self.ide_master = None,
            F::IdeSlave => self.ide_slave = None,
            F::ScsiRom => self.scsi_rom = None,
            F::ScsiRomOdd => self.scsi_rom_odd = None,
            F::ScsiUnit0 => self.scsi_units[0] = None,
            F::ScsiUnit1 => self.scsi_units[1] = None,
            F::ScsiUnit2 => self.scsi_units[2] = None,
            F::ScsiUnit3 => self.scsi_units[3] = None,
            F::ScsiUnit4 => self.scsi_units[4] = None,
            F::ScsiUnit5 => self.scsi_units[5] = None,
            F::ScsiUnit6 => self.scsi_units[6] = None,
            F::CdImage => self.cd_image = None,
            F::Cd32Nvram => self.cd32_nvram = None,
            _ => {}
        }
        // A drive's volume name is meaningless once its image is gone.
        if Self::is_drive_field(field) {
            self.set_drive_name(field, String::new());
        }
    }

    pub fn zorro_boards(&self) -> &[ZorroBoardSetup] {
        &self.zorro_boards
    }

    pub fn add_zorro(&mut self, path: PathBuf) {
        self.zorro_boards.push(ZorroBoardSetup::load(path));
    }

    pub fn remove_zorro(&mut self, idx: usize) {
        if idx < self.zorro_boards.len() {
            self.zorro_boards.remove(idx);
        }
    }

    /// Step an enum/int option on a board.
    pub fn zorro_option_cycle(&mut self, board: usize, opt: usize, forward: bool) {
        if let Some(b) = self.zorro_boards.get_mut(board) {
            b.cycle(opt, forward);
        }
    }

    /// Flip a bool option on a board.
    pub fn zorro_option_toggle(&mut self, board: usize, opt: usize) {
        if let Some(b) = self.zorro_boards.get_mut(board) {
            b.toggle(opt);
        }
    }

    /// Set a board option's value (a file path, or typed text).
    pub fn zorro_option_set(&mut self, board: usize, opt: usize, value: String) {
        if let Some(b) = self.zorro_boards.get_mut(board) {
            b.set(opt, value);
        }
    }

    /// Revert a board option to its manifest default.
    pub fn zorro_option_clear(&mut self, board: usize, opt: usize) {
        if let Some(b) = self.zorro_boards.get_mut(board) {
            b.clear(opt);
        }
    }
}

/// A short status/error line shown along the bottom of the configuration panel.
#[derive(Debug, Clone)]
pub struct StatusMessage {
    pub text: String,
    pub error: bool,
}

impl StatusMessage {
    pub fn ok(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            error: false,
        }
    }

    pub fn err(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            error: true,
        }
    }
}

/// A text field that has keyboard focus in the configuration panel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditTarget {
    /// A Zorro plugin board's string option (board index, option index).
    BoardOption { board: usize, opt: usize },
    /// A hard-drive volume-name override.
    DriveName(LauncherField),
}

/// The full interactive state of the open configuration panel.
#[derive(Debug, Clone)]
pub struct LauncherState {
    pub setup: MachineSetup,
    pub tab: LauncherTab,
    pub status: Option<StatusMessage>,
    /// The text field being typed into, and the edit buffer, when one has
    /// focus (a plugin string option or a drive volume name).
    editing: Option<EditTarget>,
    edit_buffer: String,
}

impl LauncherState {
    pub fn new(setup: MachineSetup) -> Self {
        Self {
            setup,
            tab: LauncherTab::System,
            status: None,
            editing: None,
            edit_buffer: String::new(),
        }
    }

    /// The text field currently being edited, if any.
    pub fn editing(&self) -> Option<EditTarget> {
        self.editing
    }

    /// The current edit buffer (for drawing the focused field).
    pub fn edit_buffer(&self) -> &str {
        &self.edit_buffer
    }

    /// Focus a board option for text entry, seeding the buffer with its value.
    pub fn begin_edit_board(&mut self, board: usize, opt: usize) {
        self.edit_buffer = self
            .setup
            .zorro_boards()
            .get(board)
            .map(|b| b.value(opt))
            .unwrap_or_default();
        self.editing = Some(EditTarget::BoardOption { board, opt });
        self.status = None;
    }

    /// Focus a drive's volume-name field, seeding the buffer with its value.
    pub fn begin_edit_drive_name(&mut self, field: LauncherField) {
        self.edit_buffer = self.setup.drive_name(field).unwrap_or_default().to_string();
        self.editing = Some(EditTarget::DriveName(field));
        self.status = None;
    }

    pub fn edit_push(&mut self, c: char) {
        if self.editing.is_some() {
            self.edit_buffer.push(c);
        }
    }

    pub fn edit_backspace(&mut self) {
        if self.editing.is_some() {
            self.edit_buffer.pop();
        }
    }

    /// Commit the edit buffer to the focused field.
    pub fn edit_commit(&mut self) {
        if let Some(target) = self.editing.take() {
            let value = std::mem::take(&mut self.edit_buffer);
            match target {
                EditTarget::BoardOption { board, opt } => {
                    self.setup.zorro_option_set(board, opt, value);
                }
                EditTarget::DriveName(field) => self.setup.set_drive_name(field, value),
            }
        }
    }

    pub fn edit_cancel(&mut self) {
        self.editing = None;
        self.edit_buffer.clear();
    }

    /// Open the configuration panel seeded from a raw config (the running
    /// machine, or the defaults). An invalid raw config falls back to the
    /// defaults rather than refusing to open.
    pub fn from_raw(raw: &RawConfig) -> Self {
        Self::new(MachineSetup::from_raw(raw).unwrap_or_default())
    }
}

// --- helpers --------------------------------------------------------------

fn cpu_is_32bit(cpu: CpuModel) -> bool {
    matches!(cpu, CpuModel::M68020 | CpuModel::M68030 | CpuModel::M68040)
}

/// Whether `field` appears anywhere with the given row kind. Used to classify a
/// field (toggle vs path) without threading the tab through every call.
fn rows_contains_kind(field: LauncherField, kind: RowKind) -> bool {
    TABS.iter()
        .flat_map(|&t| rows(t))
        .any(|r| r.field == field && r.kind == kind)
}

fn path_string(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

/// Build a `[ide]`/`[scsi]` drive entry from an editable path + optional
/// volume-name override. A blank name emits the bare path string so saved
/// configs stay minimal.
fn drive_raw(path: Option<&Path>, name: Option<&str>) -> Option<RawDrive> {
    path.map(|p| RawDrive {
        path: path_string(p),
        name: name
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string),
    })
}

fn cycle_slice<T: Copy + PartialEq>(items: &[T], current: T, forward: bool) -> T {
    let n = items.len();
    let idx = items.iter().position(|&x| x == current).unwrap_or(0);
    let next = if forward {
        (idx + 1) % n
    } else {
        (idx + n - 1) % n
    };
    items[next]
}

/// Cycle through float presets, snapping to the nearest preset first so a
/// loaded off-grid value still steps sensibly.
fn cycle_floats(items: &[f64], current: f64, forward: bool) -> f64 {
    let idx = nearest_index_f64(items, current);
    let n = items.len();
    let next = if forward {
        (idx + 1) % n
    } else {
        (idx + n - 1) % n
    };
    items[next]
}

/// Cycle through `usize` size presets, snapping a loaded off-grid value to the
/// nearest preset before stepping.
fn cycle_nearest(items: &[usize], current: usize, forward: bool) -> usize {
    let idx = items
        .iter()
        .enumerate()
        .min_by_key(|(_, &v)| v.abs_diff(current))
        .map(|(i, _)| i)
        .unwrap_or(0);
    let n = items.len();
    let next = if forward {
        (idx + 1) % n
    } else {
        (idx + n - 1) % n
    };
    items[next]
}

fn nearest_index_f64(items: &[f64], value: f64) -> usize {
    items
        .iter()
        .enumerate()
        .min_by(|(_, a), (_, b)| {
            (**a - value)
                .abs()
                .partial_cmp(&(**b - value).abs())
                .unwrap_or(std::cmp::Ordering::Equal)
        })
        .map(|(i, _)| i)
        .unwrap_or(0)
}

fn step_u8(current: u8, forward: bool, min: u8, max: u8) -> u8 {
    if forward {
        current.saturating_add(1).min(max)
    } else {
        current.saturating_sub(1).max(min)
    }
}

fn format_mhz(mhz: f64) -> String {
    if mhz.fract().abs() < 1e-6 {
        format!("{mhz:.0} MHz")
    } else {
        format!("{mhz:.2} MHz")
    }
}

fn size_label(bytes: usize) -> String {
    if bytes == 0 {
        "None".to_string()
    } else {
        format_size(bytes)
    }
}

// Names that round-trip through the config parsers (used by to_raw).

fn model_name(model: MachineModel) -> &'static str {
    match model {
        MachineModel::A500 => "A500",
        MachineModel::A500Ocs => "A500OCS",
        MachineModel::A500Plus => "A500Plus",
        MachineModel::A600 => "A600",
        MachineModel::A1200 => "A1200",
        MachineModel::Cdtv => "CDTV",
        MachineModel::Cd32 => "CD32",
        MachineModel::A1000 => "A1000",
    }
}

/// Friendlier label for the model selector buttons.
pub fn model_label(model: MachineModel) -> &'static str {
    match model {
        MachineModel::A500 => "A500",
        MachineModel::A500Ocs => "A500 OCS",
        MachineModel::A500Plus => "A500+",
        MachineModel::A600 => "A600",
        MachineModel::A1200 => "A1200",
        MachineModel::Cdtv => "CDTV",
        MachineModel::Cd32 => "CD32",
        MachineModel::A1000 => "A1000",
    }
}

fn chipset_name(chipset: Chipset) -> &'static str {
    match chipset {
        Chipset::Ocs => "OCS",
        Chipset::Ecs => "ECS",
        Chipset::Aga => "AGA",
    }
}

fn cpu_name(cpu: CpuModel) -> &'static str {
    match cpu {
        CpuModel::M68000 => "68000",
        CpuModel::M68EC020 => "68EC020",
        CpuModel::M68020 => "68020",
        CpuModel::M68030 => "68030",
        CpuModel::M68040 => "68040",
    }
}

fn agnus_name(agnus: AgnusRevision) -> &'static str {
    match agnus {
        AgnusRevision::Ocs => "OCS",
        AgnusRevision::Ecs8372Rev4 => "8372A",
        AgnusRevision::Ecs8375 => "8375",
        AgnusRevision::AgaAlice => "ALICE",
    }
}

fn denise_name(denise: DeniseRevision) -> &'static str {
    match denise {
        DeniseRevision::Ocs => "OCS",
        DeniseRevision::Ecs8373 => "ECS",
        DeniseRevision::AgaLisa => "LISA",
    }
}

fn video_name(video: VideoStandard) -> &'static str {
    match video {
        VideoStandard::Pal => "PAL",
        VideoStandard::Ntsc => "NTSC",
    }
}

fn overscan_name(overscan: Overscan) -> &'static str {
    match overscan {
        Overscan::Tv => "tv",
        Overscan::Full => "full",
    }
}

fn pixel_aspect_name(aspect: PixelAspect) -> &'static str {
    match aspect {
        PixelAspect::Tv => "tv",
        PixelAspect::Square => "square",
    }
}

fn pacing_name(pacing: PacingBudget) -> &'static str {
    match pacing {
        PacingBudget::Cycles => "cycles",
        PacingBudget::Instructions => "instructions",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_board_manifest() -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "copperline-launcher-board-{}-{}.toml",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::write(
            &path,
            r#"
            name = "Test Plugin"
            zorro = 2
            type = "wasm"
            size = "64K"
            manufacturer = 5192
            product = 16
            wasm = "x.wasm"
            [config]
            speed = "fast"
            verbose = false
            [[option]]
            key = "speed"
            label = "Speed"
            type = "enum"
            choices = ["slow", "fast"]
            [[option]]
            key = "verbose"
            label = "Verbose"
            type = "bool"
            [[option]]
            key = "count"
            label = "Count"
            type = "int"
            default = 3
            [[option]]
            key = "rom"
            label = "ROM"
            type = "file"
        "#,
        )
        .unwrap();
        path
    }

    #[test]
    fn plugin_board_options_load_edit_and_round_trip() {
        let path = write_board_manifest();
        let mut board = ZorroBoardSetup::load(path.clone());
        assert_eq!(board.options().len(), 4);
        // Defaults: [config] for speed/verbose, the option default for count.
        assert_eq!(board.value(0), "fast");
        assert_eq!(board.value(1), "false");
        assert_eq!(board.value(2), "3");
        assert_eq!(board.value(3), ""); // unset file

        board.cycle(0, true); // enum fast -> slow (wraps)
        assert_eq!(board.value(0), "slow");
        board.toggle(1); // bool false -> true
        assert_eq!(board.value(1), "true");
        board.cycle(2, false); // int 3 -> 2
        assert_eq!(board.value(2), "2");
        board.set(3, "/tmp/board.rom".into());
        assert_eq!(board.value(3), "/tmp/board.rom");
        board.clear(2); // revert int to its default
        assert_eq!(board.value(2), "3");

        // Overrides serialize back, typed per the option schema.
        let setup = MachineSetup {
            zorro_boards: vec![board],
            ..MachineSetup::default()
        };
        let raw = setup.to_raw();
        let cfg = raw.zorro[0].config.as_ref().expect("overrides emitted");
        assert_eq!(cfg.get("speed").unwrap().as_str(), Some("slow"));
        assert_eq!(cfg.get("verbose").unwrap().as_bool(), Some(true));
        assert_eq!(cfg.get("rom").unwrap().as_str(), Some("/tmp/board.rom"));
        // "count" was reverted to default, so it is not emitted.
        assert!(cfg.get("count").is_none());

        // And those overrides round-trip back through from_raw.
        let reloaded = MachineSetup::from_raw(&raw).unwrap();
        assert_eq!(reloaded.zorro_boards()[0].value(0), "slow");
        assert_eq!(reloaded.zorro_boards()[0].value(1), "true");

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn default_setup_is_the_a500_aros_machine() {
        let s = MachineSetup::default();
        assert_eq!(s.model, None);
        // With no profile chosen the picker highlights the A500 (the default
        // machine is the A500 defaults).
        assert_eq!(s.selected_model(), MachineModel::A500);
        assert_eq!(s.chipset, Chipset::Ecs);
        assert_eq!(s.cpu, CpuModel::M68000);
        assert_eq!(s.chip_ram, 512 * 1024);
        assert_eq!(s.slow_ram, 512 * 1024);
        assert!(s.rom.is_none(), "boot ROM defaults to bundled AROS");
        // The base A500 had no battery-backed clock.
        assert!(!s.toggle_value(LauncherField::Rtc));
        // The greyed Zorro III RAM explains why on this 24-bit machine.
        assert_eq!(
            s.disabled_reason(LauncherField::Z3Ram),
            Some("needs 32-bit CPU")
        );
        // A bare default emits no overrides at all.
        let toml = s.to_toml().unwrap();
        assert!(toml.trim().is_empty(), "expected empty TOML, got:\n{toml}");
        assert!(s.build_config().is_ok());
    }

    #[test]
    fn launcher_exposes_both_cache_toggles_for_the_68040() {
        let mut s = MachineSetup::default();
        // Step the CPU selector along to the 68040.
        for _ in 0..CPUS.len() {
            if s.cpu == CpuModel::M68040 {
                break;
            }
            s.cycle(LauncherField::Cpu, true);
        }
        assert_eq!(s.cpu, CpuModel::M68040, "cycled to the 68040");
        // The 040 has both caches, so neither toggle is greyed and both default
        // on (like the 030) when the part is selected.
        assert_eq!(s.disabled_reason(LauncherField::Icache), None);
        assert_eq!(s.disabled_reason(LauncherField::Dcache), None);
        assert!(s.toggle_value(LauncherField::Icache));
        assert!(s.toggle_value(LauncherField::Dcache));

        // The 68000 has neither; the 68EC020 has only the instruction cache.
        s.cpu = CpuModel::M68000;
        assert!(s.disabled_reason(LauncherField::Icache).is_some());
        assert!(s.disabled_reason(LauncherField::Dcache).is_some());
        s.cpu = CpuModel::M68EC020;
        assert_eq!(s.disabled_reason(LauncherField::Icache), None);
        assert!(s.disabled_reason(LauncherField::Dcache).is_some());
    }

    #[test]
    fn select_model_applies_profile_defaults_and_emits_only_the_profile() {
        let mut s = MachineSetup::default();
        s.select_model(Some(MachineModel::A1200));
        assert_eq!(s.chipset, Chipset::Aga);
        assert_eq!(s.cpu, CpuModel::M68EC020);
        assert_eq!(s.chip_ram, 2 * 1024 * 1024);
        // The base A1200 shipped without a populated RTC; the A500+ has one.
        assert!(!s.toggle_value(LauncherField::Rtc));
        s.select_model(Some(MachineModel::A500Plus));
        assert!(s.toggle_value(LauncherField::Rtc));
        s.select_model(Some(MachineModel::A1200));
        let raw = s.to_raw();
        assert_eq!(raw.machine.profile.as_deref(), Some("A1200"));
        // Everything else matches the profile default, so nothing else is set.
        assert!(raw.memory.chip.is_none());
        assert!(raw.cpu.model.is_none());
        assert!(raw.chipset.revision.is_none());
        assert!(s.build_config().is_ok());
    }

    #[test]
    fn cycling_chip_ram_walks_the_presets() {
        let mut s = MachineSetup::default();
        assert_eq!(s.chip_ram, 512 * 1024);
        s.cycle(LauncherField::ChipRam, true);
        assert_eq!(s.chip_ram, 1024 * 1024);
        s.cycle(LauncherField::ChipRam, true);
        assert_eq!(s.chip_ram, 2 * 1024 * 1024);
        s.cycle(LauncherField::ChipRam, false);
        assert_eq!(s.chip_ram, 1024 * 1024);
    }

    #[test]
    fn agnus_override_round_trips_through_raw() {
        let mut s = MachineSetup::default();
        s.cycle(LauncherField::Agnus, true); // None -> Some(OCS)
        assert_eq!(s.agnus, Some(AgnusRevision::Ocs));
        let raw = s.to_raw();
        assert_eq!(raw.chipset.agnus.as_deref(), Some("OCS"));
        let back = MachineSetup::from_raw(&raw).unwrap();
        assert_eq!(back.agnus, Some(AgnusRevision::Ocs));
    }

    #[test]
    fn joystick_input_mode_round_trips_through_raw() {
        let mut s = MachineSetup::default();
        // Default is Gamepad, which emits no [input] section.
        assert_eq!(s.joystick_input_mode, JoystickInputMode::Gamepad);
        assert!(s.to_raw().input.joystick.is_none());
        // The stepper flips between the two explicit modes.
        s.cycle(LauncherField::Joystick, true);
        assert_eq!(s.joystick_input_mode, JoystickInputMode::Keyboard);
        let raw = s.to_raw();
        assert_eq!(raw.input.joystick.as_deref(), Some("keyboard"));
        let back = MachineSetup::from_raw(&raw).unwrap();
        assert_eq!(back.joystick_input_mode, JoystickInputMode::Keyboard);
        s.cycle(LauncherField::Joystick, true);
        assert_eq!(s.joystick_input_mode, JoystickInputMode::Gamepad);
        // Switching machine profile resets it to the Gamepad default.
        let mut s = MachineSetup::default();
        s.cycle(LauncherField::Joystick, true);
        s.select_model(Some(MachineModel::A1200));
        assert_eq!(s.joystick_input_mode, JoystickInputMode::Gamepad);
    }

    #[test]
    fn build_config_surfaces_validation_errors() {
        // Z3 RAM on a 68000 (24-bit bus) is rejected by the config validator;
        // the model leans on that rather than re-checking.
        let mut s = MachineSetup::default();
        s.cycle(LauncherField::Z3Ram, true);
        assert_eq!(s.z3_ram, 16 * 1024 * 1024);
        let err = s.build_config().unwrap_err().to_string();
        assert!(err.contains("Zorro III"), "{err}");
    }

    #[test]
    fn setting_a_floppy_path_round_trips_and_wires_the_drive() {
        let mut s = MachineSetup::default();
        s.set_path(LauncherField::Df1Image, PathBuf::from("/disks/b.adf"));
        assert!(s.floppy_drives >= 2, "DF1 media wires in a second drive");
        let raw = s.to_raw();
        assert_eq!(raw.floppy.drives, Some(2));
        assert_eq!(
            raw.floppy.df1.as_ref().and_then(|d| d.path.as_deref()),
            Some("/disks/b.adf")
        );
    }

    #[test]
    fn drive_volume_name_round_trips_through_raw() {
        let mut s = MachineSetup::default();
        s.select_model(Some(MachineModel::A1200)); // Gayle, so IDE applies.
        s.set_path(LauncherField::IdeMaster, PathBuf::from("/host/games"));
        s.set_drive_name(LauncherField::IdeMaster, "Games".to_string());
        assert_eq!(s.drive_name(LauncherField::IdeMaster), Some("Games"));

        let raw = s.to_raw();
        let master = raw.ide.master.as_ref().expect("master emitted");
        assert_eq!(master.path, "/host/games");
        assert_eq!(master.name.as_deref(), Some("Games"));

        let back = MachineSetup::from_raw(&raw).unwrap();
        assert_eq!(back.drive_name(LauncherField::IdeMaster), Some("Games"));
    }

    #[test]
    fn drive_volume_name_without_an_image_is_dropped() {
        let mut s = MachineSetup::default();
        s.select_model(Some(MachineModel::A1200));
        // No image set: a name has nothing to label.
        s.set_drive_name(LauncherField::IdeMaster, "Orphan".to_string());
        assert_eq!(s.drive_name(LauncherField::IdeMaster), None);

        // With an image the name sticks, then clearing the image drops it too.
        s.set_path(LauncherField::IdeMaster, PathBuf::from("/host/games"));
        s.set_drive_name(LauncherField::IdeMaster, "Games".to_string());
        assert_eq!(s.drive_name(LauncherField::IdeMaster), Some("Games"));
        s.clear_path(LauncherField::IdeMaster);
        assert_eq!(s.drive_name(LauncherField::IdeMaster), None);
    }

    #[test]
    fn editing_a_drive_name_commits_to_the_setup() {
        let mut setup = MachineSetup::default();
        setup.select_model(Some(MachineModel::A1200));
        setup.set_path(LauncherField::ScsiUnit0, PathBuf::from("/host/work"));
        let mut state = LauncherState::new(setup);
        state.begin_edit_drive_name(LauncherField::ScsiUnit0);
        for ch in "WORK".chars() {
            state.edit_push(ch);
        }
        state.edit_commit();
        assert_eq!(state.editing(), None);
        assert_eq!(
            state.setup.drive_name(LauncherField::ScsiUnit0),
            Some("WORK")
        );
    }
}
