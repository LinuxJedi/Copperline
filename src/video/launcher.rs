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

use crate::bus::PortDevice;
use crate::chipset::agnus::{AgnusRevision, VideoStandard};
use crate::chipset::denise::DeniseRevision;
use crate::config::{
    format_size, machine_profile_defaults, AudioFilterMode, BezelStyle, BridgeCable, BridgeDensity,
    BridgeDriver, BridgeReadMode, ChannelMode, Chipset, Config, CpuModel, DisplayScaling,
    FluxBridgeConfig, JoystickInputMode, MachineModel, MenuScale, MouseCapture, Mt32Lcd, Overscan,
    PacingBudget, ParallelDevice, PixelAspect, RawConfig, RawDrive, RawFilesysMount,
    RawFloppyDrive, RawHostDisk, RawZorroBoard, RtgCard, ScsiController, SerialMode, ShaderMode,
    Tint, WarpSpeed, BOOT_PRI_NEVER,
};
use crate::ide_zorro::LidePersonality;
use crate::memory::{RamInit, DEFAULT_RAM_PATTERN, DEFAULT_RANDOM_RAM_SEED};
use crate::net::NetConfig;
use crate::zorro::{ConfigOption, ConfigOptionKind, LoadedZorroBoard};
use anyhow::Result;
use std::borrow::Cow;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

mod fields;
mod netplay;
pub use netplay::NetplaySetup;
mod setup_config;
mod values;
pub use fields::*;
use LauncherField as F;
use RowKind::Path as PathRow;
use RowKind::{Bootpri, Cycle, Drive, Toggle};

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
}

/// The real disks a loaded configuration already gives the machine.
fn raw_host_disks(raw: &RawConfig) -> Vec<crate::config::HostDiskConfig> {
    raw.host_disk
        .iter()
        .filter(|entry| !entry.device.trim().is_empty())
        .map(|entry| crate::config::HostDiskConfig {
            device: entry.device.trim().to_string(),
            fingerprint: entry.fingerprint.clone(),
            identity_confirmed: false,
            attach: entry
                .attach
                .as_deref()
                .map(str::trim)
                .and_then(|token| {
                    crate::config::HostDiskAttach::all()
                        .iter()
                        .copied()
                        .find(|a| a.token().eq_ignore_ascii_case(token))
                })
                .unwrap_or_default(),
            writable: !entry.read_only.unwrap_or(true),
        })
        .collect()
}

/// One line of the Host Disk table.
///
/// Flattened for drawing: the page shows text, and the work of deciding what
/// a device is called and whether it may be touched belongs to the layer that
/// knows about hardware, not to the launcher.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostDiskRow {
    /// The identifier the host and the configuration both use: `disk4`,
    /// `sdb`, `PhysicalDrive1`.
    pub id: String,
    /// Opaque hardware identity, when this is a real enumerated disk.
    pub fingerprint: Option<String>,
    /// What the hardware calls itself, or the volume on it.
    pub volume: String,
    /// Capacity, already rounded for reading.
    pub size: String,
    /// Where the host currently has this disk mounted, if anywhere. Shown so
    /// somebody can see why a disk cannot simply be taken.
    pub mounted: Vec<String>,
    /// Whether the guest may write to it. Off by default; writable real-media
    /// access is an explicit choice.
    pub writable: bool,
    /// Where the machine would see this disk -- and only while it is ticked.
    /// An unticked disk is going nowhere, and its cell reads blank rather
    /// than naming a place nothing has claimed.
    pub attach: Option<crate::config::HostDiskAttach>,
}

/// The disks the Host Disk page will offer.
///
/// Every whole device the host has, less the one it is running from. A drive
/// on a SATA port is as usable as one in a card reader, and the emulator is
/// in no position to say which somebody meant -- so an internal disk is
/// labelled rather than withheld, and the refusal is kept for the case that
/// is never right. `blockdev` is what decides that, following a synthesized
/// root (an APFS container, an LVM volume) down to the hardware it lives on.
#[cfg(not(target_arch = "wasm32"))]
fn sample_host_disks() -> Vec<HostDiskRow> {
    crate::blockdev::list_devices()
        .unwrap_or_default()
        .into_iter()
        .filter(|device| device.safety.listable())
        .map(|device| HostDiskRow {
            fingerprint: Some(device.fingerprint()),
            volume: device.model.clone().unwrap_or_else(|| device.id.clone()),
            size: device.size_label(),
            mounted: device.mounted.clone(),
            id: device.id,
            writable: false,
            attach: None,
        })
        .chain(fake_host_disks())
        .collect()
}

#[cfg(target_arch = "wasm32")]
fn sample_host_disks() -> Vec<HostDiskRow> {
    Vec::new()
}

/// Invented disks appended to the real ones, for seeing how a long list
/// behaves without owning a drawer of card readers. `COPPERLINE_FAKE_DISKS=20`
/// adds twenty; unset, nothing changes. Diagnostic only -- they cannot be
/// opened, so mounting one fails as any absent disk does.
fn fake_host_disks() -> Vec<HostDiskRow> {
    let Some(count) = crate::envcfg::var_os("COPPERLINE_FAKE_DISKS") else {
        return Vec::new();
    };
    let count: usize = count.to_string_lossy().trim().parse().unwrap_or(0);
    (0..count.min(64))
        .map(|i| HostDiskRow {
            id: format!("fakedisk{i}"),
            fingerprint: None,
            volume: format!("Pretend Media {i}"),
            size: format!("{}.0 GB", i % 9 + 1),
            mounted: Vec::new(),
            writable: false,
            attach: None,
        })
        .collect()
}

/// How close a bay is to actually having a physical drive behind it.
///
/// The library is compiled in, so the only thing that can be missing is the
/// hardware itself; the launcher says so plainly rather than "None".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BridgeStatus {
    /// Nothing the bridge recognises is plugged in. The bridge itself is
    /// always there in a build with the feature, and a build without one never
    /// shows a bay that could ask.
    NoInterface,
    /// An interface the bridge recognises is attached.
    Attached,
}

/// Every serial device the host offers beyond the library's own scan. macOS:
/// the callout (`cu.*`) devices -- the class made for originating
/// connections, and the one that still names chips the scan's `tty.usb*`
/// filter misses (an Arduino clone on a CH340 mounts as `tty.wchusbserial*`).
/// Linux: the USB serial classes. Windows: nothing extra -- the library
/// already walks every COM port through SetupAPI.
#[cfg(feature = "fluxbridge")]
fn host_serial_ports() -> Vec<String> {
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    {
        let Ok(dir) = std::fs::read_dir("/dev") else {
            return Vec::new();
        };
        let wanted = |name: &str| {
            if cfg!(target_os = "macos") {
                name.starts_with("cu.")
            } else {
                name.starts_with("ttyACM") || name.starts_with("ttyUSB")
            }
        };
        let mut ports: Vec<String> = dir
            .filter_map(|e| e.ok())
            .filter_map(|e| e.file_name().into_string().ok())
            .filter(|n| wanted(n))
            .map(|n| format!("/dev/{n}"))
            .collect();
        ports.sort();
        ports
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        Vec::new()
    }
}

/// "Automatic", then the library's scan in its own order, then the host
/// devices it did not name.
///
/// Only the FluxBridge page has a port to choose, so without the feature
/// there is nothing to merge. A macOS device counts as already named when the
/// scan lists its `tty.` twin: `cu.usbmodem101` and `tty.usbmodem101` are
/// one port, and the scan's spelling wins.
#[cfg(feature = "fluxbridge")]
fn merge_port_lists(library: Vec<String>, host: Vec<String>) -> Vec<Option<String>> {
    let mut out: Vec<Option<String>> = std::iter::once(None)
        .chain(library.iter().cloned().map(Some))
        .collect();
    for port in host {
        let twin = port
            .strip_prefix("/dev/cu.")
            .map(|rest| format!("/dev/tty.{rest}"));
        let named = library
            .iter()
            .any(|l| *l == port || twin.as_deref() == Some(l.as_str()));
        if !named {
            out.push(Some(port));
        }
    }
    out
}

/// The ports the bridge page offers: what the library scanned, plus whatever
/// else the host has.
#[cfg(feature = "fluxbridge")]
fn sample_bridge_ports() -> Vec<Option<String>> {
    merge_port_lists(crate::fluxbridge::com_ports(), host_serial_ports())
}

/// What the bridge can see of the host right now.
fn bridge_status() -> BridgeStatus {
    #[cfg(feature = "fluxbridge")]
    if crate::fluxbridge::interface_connected() {
        return BridgeStatus::Attached;
    }
    BridgeStatus::NoInterface
}

/// Config-file spellings for the bridge settings, matching what the parser
/// accepts.
fn bridge_driver_name(d: BridgeDriver) -> &'static str {
    match d {
        BridgeDriver::DrawBridge => "drawbridge",
        BridgeDriver::Greaseweazle => "greaseweazle",
        BridgeDriver::SupercardPro => "supercardpro",
    }
}

fn bridge_cable_name(c: BridgeCable) -> &'static str {
    match c {
        BridgeCable::DriveA => "a",
        BridgeCable::DriveB => "b",
        BridgeCable::Shugart0 => "0",
        BridgeCable::Shugart1 => "1",
        BridgeCable::Shugart2 => "2",
        BridgeCable::Shugart3 => "3",
    }
}

fn bridge_density_name(d: BridgeDensity) -> &'static str {
    match d {
        BridgeDensity::Auto => "auto",
        BridgeDensity::Dd => "dd",
        BridgeDensity::Hd => "hd",
    }
}

fn bridge_mode_name(m: BridgeReadMode) -> &'static str {
    match m {
        BridgeReadMode::Compatible => "compatible",
        BridgeReadMode::Normal => "normal",
        BridgeReadMode::Stalling => "stalling",
    }
}

/// The interfaces the Interface row offers, straight from the FluxBridge
/// build: the library's own driver table decides what exists, in its own
/// order, so compiling a driver in is the whole job of adding it here.
#[cfg(feature = "fluxbridge")]
fn bridge_drivers() -> Vec<BridgeDriver> {
    crate::fluxbridge::drivers()
        .iter()
        .filter_map(|driver| {
            [
                BridgeDriver::DrawBridge,
                BridgeDriver::Greaseweazle,
                BridgeDriver::SupercardPro,
            ]
            .into_iter()
            .find(|kind| kind.match_token() == driver.token)
        })
        .collect()
}

/// Without the bridge the page is unreachable, but the row still needs a
/// value to show.
#[cfg(not(feature = "fluxbridge"))]
fn bridge_drivers() -> Vec<BridgeDriver> {
    vec![BridgeDriver::default()]
}
const BRIDGE_CABLES: [BridgeCable; 6] = [
    BridgeCable::DriveA,
    BridgeCable::DriveB,
    BridgeCable::Shugart0,
    BridgeCable::Shugart1,
    BridgeCable::Shugart2,
    BridgeCable::Shugart3,
];
const BRIDGE_DENSITIES: [BridgeDensity; 3] =
    [BridgeDensity::Auto, BridgeDensity::Dd, BridgeDensity::Hd];
// The driver's fourth mode, Turbo, is absent: it answers AmigaDOS calls
// instead of reading the disk, so there is nothing for a drive model to do
// with it.
const BRIDGE_READ_MODES: [BridgeReadMode; 3] = [
    BridgeReadMode::Normal,
    BridgeReadMode::Compatible,
    BridgeReadMode::Stalling,
];
const AUDIO_FILTER_MODES: [AudioFilterMode; 3] = [
    AudioFilterMode::Auto,
    AudioFilterMode::On,
    AudioFilterMode::Off,
];
const FLOPPY_SPEEDS: [u16; 5] = [100, 200, 400, 800, crate::floppy::SPEED_TURBO];
// The bridge speed row cycles the same set the config and CLI accept.
use crate::config::SUPPORTED_BRIDGE_SPEED_PERCENTS as BRIDGE_REPLAY_SPEEDS;
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
// When the host mouse is grabbed, in stepper order.
const MOUSE_CAPTURES: [MouseCapture; 3] = [
    MouseCapture::Click,
    MouseCapture::Auto,
    MouseCapture::Manual,
];
// Controller devices a game port accepts, in stepper order.
const PORT_DEVICES: [PortDevice; 5] = [
    PortDevice::Mouse,
    PortDevice::Joystick,
    PortDevice::Cd32Pad,
    PortDevice::Analogue,
    PortDevice::None,
];
// Port 1 offers one more: a mouse a gamepad can move as well as the
// hand on the desk. Only port 1, because only port 1 is where a mouse
// belongs -- Workbench and nearly every game read it there.
const PORT1_DEVICES: [PortDevice; 6] = [
    PortDevice::Mouse,
    PortDevice::GamepadMouse,
    PortDevice::Joystick,
    PortDevice::Cd32Pad,
    PortDevice::Analogue,
    PortDevice::None,
];
// `None` = no SCSI board fitted; the two boards are mutually exclusive here even
// though the engine could run both, so a config round-trips through this picker.
const SCSI_CONTROLLERS: [Option<ScsiController>; 4] = [
    None,
    Some(ScsiController::A2091),
    Some(ScsiController::A4091),
    Some(ScsiController::A3000),
];
// `None` = no lide board fitted. Unlike SCSI's boards, all three personalities
// work on any machine model, so there is no per-model filtering to do here.
const LIDE_BOARDS: [Option<LidePersonality>; 4] = [
    None,
    Some(LidePersonality::Ripple),
    Some(LidePersonality::Ride),
    Some(LidePersonality::AtBus2008),
];
#[cfg(feature = "midi")]
const SERIAL_MODES: [SerialMode; 7] = [
    SerialMode::Off,
    SerialMode::Stdout,
    SerialMode::Midi,
    SerialMode::Tcp,
    SerialMode::TcpConnect,
    SerialMode::Pty,
    SerialMode::Modem,
];
/// Stereo-separation presets the picker steps through (percent), ascending so
/// the right arrow steps up (wrapping 100 -> 0) and the left arrow steps down.
/// The config/CLI accept any 0-100; an off-grid value snaps to the nearest here.
const STEREO_SEPARATION_STEPS: [usize; 11] = [0, 10, 20, 30, 40, 50, 60, 70, 80, 90, 100];

/// Sampler input-gain presets the picker steps through (preamp decibels, in
/// 3 dB steps), ascending. 0 dB is unity; the ends are the sampler's
/// [`crate::sampler::MIN_SAMPLER_GAIN_DB`]..[`crate::sampler::MAX_SAMPLER_GAIN_DB`].
/// The config/CLI accept any value in range; an off-grid value snaps to the
/// nearest here.
const SAMPLER_GAIN_STEPS: [f64; 17] = [
    -24.0, -21.0, -18.0, -15.0, -12.0, -9.0, -6.0, -3.0, 0.0, 3.0, 6.0, 9.0, 12.0, 15.0, 18.0,
    21.0, 24.0,
];

/// Label a sampler gain in decibels, e.g. `0 dB`, `+6 dB`, `-12 dB`.
fn sampler_gain_label(gain_db: f32) -> String {
    if gain_db.abs() < 0.05 {
        "0 dB".to_string()
    } else {
        format!("{gain_db:+.0} dB")
    }
}

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
    rtg: RtgCard,
    /// Memory fitted to a configurable RTG board. The launcher currently
    /// preserves this value from loaded configs; the card selector does not
    /// need a separate control for the two period-correct Picasso II/II+ sizes.
    rtg_vram_bytes: usize,
    // CPU
    cpu: CpuModel,
    fpu: bool,
    clock_mhz: f64,
    icache: bool,
    dcache: bool,
    /// Fast batch/trace-JIT CPU execution (`[cpu] jit`); not cycle-exact.
    jit: bool,
    // Memory (bytes)
    chip_ram: usize,
    fast_ram: usize,
    slow_ram: usize,
    /// Diagnostic cold-start policy selected by the Memory page.
    ram_init: RamInit,
    /// Values remembered while another fill mode is selected, so cycling away
    /// from a loaded/custom mode and back does not silently replace its value.
    ram_pattern: u16,
    ram_random_seed: u64,
    mb_ram: usize,
    accel_ram: usize,
    z3_ram: usize,
    // ROM (None = bundled default for the boot and CD32 FMV ROMs)
    rom: Option<PathBuf>,
    extended_rom: Option<PathBuf>,
    fmv_rom: Option<PathBuf>,
    /// An explicit `fmv_rom = ""` leaves the CD32 cartridge slot empty.
    fmv_rom_disabled: bool,
    // Floppy
    floppy_drives: u8,
    /// `[floppy] speed`: a percentage (100/200/400/800) or 0 for turbo.
    floppy_speed: u16,
    /// Per-drive disk-swap playlists (entry 0 is the boot disk). A single
    /// image is a one-element list.
    df_playlists: [Vec<PathBuf>; 4],
    df_write_protected: [bool; 4],
    /// A real drive on this bay instead of an image. `None` is the ordinary
    /// image-backed drive.
    df_bridge: [Option<FluxBridgeConfig>; 4],
    /// The bay's interface is set to "None": still a physical-drive bay in
    /// the launcher, but with no interface to drive it -- the run and the
    /// written config treat it as unbridged. Selected automatically when a
    /// bay is bridged with nothing attached.
    df_bridge_none: [bool; 4],
    /// Which bay the FluxBridge settings page is showing. The page itself is
    /// one set of rows; this says whose values they are.
    bridge_edit_drive: usize,
    /// What the library could see the last time we looked. Sampled when the
    /// launcher opens and whenever a bay is switched over to a physical drive,
    /// so a board plugged in mid-session is picked up by unticking and
    /// re-ticking the box rather than by a scan every frame.
    bridge_status: BridgeStatus,
    /// The serial ports on offer -- "Automatic", the library's scan, then
    /// every host serial device the scan did not name. Sampled with
    /// `bridge_status` (launcher open, a bay switched to a physical drive):
    /// the scan walks the serial bus, which is not a per-frame activity.
    #[cfg(feature = "fluxbridge")]
    bridge_ports: Vec<Option<String>>,
    /// Host disks the Host Disk page can offer, sampled when that page is
    /// opened or refreshed rather than every frame: enumerating walks the
    /// host's storage tree, and a card pushed in mid-session is picked up by
    /// Refresh rather than by polling.
    host_disks: Vec<HostDiskRow>,
    /// First row shown in the disk table. The list can be longer than the
    /// box, and a disk that cannot be scrolled to cannot be chosen.
    host_disk_scroll: usize,
    /// How fast a held scroll over that table is running.
    host_disk_scroll_rate: ScrollRate,
    /// Why the last tick was refused. Read once by the caller, which puts it
    /// on the status line with every other warning; cleared by the next tick,
    /// because the situation it describes is the one being changed.
    host_disk_warning: Option<String>,
    /// The disks ticked in the table. A machine can take several at once, so
    /// long as no two want the same place; ticking one claims the first place
    /// still free. Seeded from what is already attached, so leaving the page
    /// and coming back shows the machine as it stands rather than as new.
    host_disk_selected: Vec<String>,
    /// Real disks given to the machine, at most one per attachment point.
    /// Held in the configuration's own shape: this is a setting that
    /// persists, not a fact about this session.
    host_disks_attached: Vec<crate::config::HostDiskConfig>,
    // Hard disk. Each drive's optional volume-name override (directory mounts
    // only) sits in the matching `*_name` slot, paralleling the path slot. Boot
    // priority is edited on the Boot Priority sub-page and split across two
    // slots: `*_bootpri` holds the priority (None = unset, shown as 0), and
    // `*_boot_off` is the Bootable checkbox cleared -- the config's -128 (never)
    // sentinel. They are separate so unticking Bootable greys the priority
    // without discarding the number within the session (the config can only
    // store one or the other, so a save of a disabled drive still writes -128).
    ide_master: Option<PathBuf>,
    ide_master_name: Option<String>,
    /// The filesystem an in-memory directory-mount volume is built with
    /// (meaningless, and never emitted, unless the path is a directory);
    /// paralleling `*_name` above.
    ide_master_fs: crate::diskimage::FileSystem,
    /// Whether `ide_master`'s path is a host directory, sampled whenever
    /// the path is set or loaded rather than on every frame the row is
    /// drawn: `Path::is_dir()` is a host filesystem call that can stall on
    /// a slow or disconnected mount, and the FFS/OFS toggle's visibility
    /// (`launcher_drive_fs_applies`, `ui.rs`) needs it every frame that row
    /// is on screen.
    ide_master_is_dir: bool,
    ide_master_bootpri: Option<i8>,
    ide_master_boot_off: bool,
    ide_slave: Option<PathBuf>,
    ide_slave_name: Option<String>,
    ide_slave_fs: crate::diskimage::FileSystem,
    ide_slave_is_dir: bool,
    ide_slave_bootpri: Option<i8>,
    ide_slave_boot_off: bool,
    /// Which SCSI host adapter is fitted, or `None` for no board. Shares the
    /// `scsi_*` ROM/unit block below (the drives are portable between boards).
    scsi_controller: Option<ScsiController>,
    scsi_rom: Option<PathBuf>,
    scsi_rom_odd: Option<PathBuf>,
    scsi_units: [Option<PathBuf>; 7],
    scsi_unit_names: [Option<String>; 7],
    scsi_unit_fs: [crate::diskimage::FileSystem; 7],
    /// Paralleling `ide_master_is_dir`, per unit.
    scsi_unit_is_dir: [bool; 7],
    scsi_unit_bootpri: [Option<i8>; 7],
    scsi_unit_boot_off: [bool; 7],
    /// `[copperhf]` unit images, by unit number (0-6). Unlike `[scsi]` there
    /// is no controller/ROM to fit first -- the board is always there --
    /// so these seven are the whole of it.
    copperhf_units: [Option<PathBuf>; 7],
    copperhf_unit_names: [Option<String>; 7],
    copperhf_unit_fs: [crate::diskimage::FileSystem; 7],
    /// Paralleling `ide_master_is_dir`, per unit.
    copperhf_unit_is_dir: [bool; 7],
    copperhf_unit_bootpri: [Option<i8>; 7],
    copperhf_unit_boot_off: [bool; 7],
    /// Which lide personality is fitted, or `None` for no board. Unlike
    /// `[scsi]`, presence is inferred from the config's own `rom`/`drives`
    /// (see `LideConfig::enabled`); this `Option` is purely the launcher's
    /// session-level "is a board fitted" toggle.
    lide_board: Option<LidePersonality>,
    /// `None` here reads as "use the bundled default", the same as an
    /// absent `rom` key -- the sentinel `Config` resolves it to is never
    /// let into this field. `lide_rom_disabled` carries the config's
    /// explicit `rom = ""` opt-out (hardware-only mode) through a launcher
    /// session that never touches the field; typing a path clears it.
    lide_rom: Option<PathBuf>,
    lide_rom_disabled: bool,
    lide_rom_bank2: Option<PathBuf>,
    lide_rom_bank2_disabled: bool,
    /// Drive slots in (channel, master/slave) order (RIPPLE's two channels;
    /// RIDE/AT-Bus 2008 only ever use the first two). `[lide] drives` is a
    /// positional list in the config file -- a hole cannot be represented --
    /// so `clear_path` cascades: clearing slot N also clears every slot
    /// after it, keeping this array always representable as a config.
    lide_drives: [Option<PathBuf>; 4],
    lide_drive_names: [Option<String>; 4],
    lide_drive_fs: [crate::diskimage::FileSystem; 4],
    /// Paralleling `ide_master_is_dir`, per drive.
    lide_drive_is_dir: [bool; 4],
    lide_drive_bootpri: [Option<i8>; 4],
    lide_drive_boot_off: [bool; 4],
    // Host FS mounts. The GUI edits the first FILESYS_GUI_SLOTS entries
    // (directory + optional volume name + boot priority, -128 = never boot);
    // any further hand-written [[filesys]] entries are carried in
    // `filesys_extra` and re-emitted verbatim so a save never drops them.
    filesys_dirs: [Option<PathBuf>; FILESYS_GUI_SLOTS],
    filesys_names: [Option<String>; FILESYS_GUI_SLOTS],
    filesys_bootpri: [i8; FILESYS_GUI_SLOTS],
    filesys_readonly: [bool; FILESYS_GUI_SLOTS],
    filesys_extra: Vec<RawFilesysMount>,
    // CD
    cd_image: Option<PathBuf>,
    cd_insert_delay: f64,
    cd32_nvram: Option<PathBuf>,
    // WHDLoad direct boot (`[whdload]`, src/whdload.rs): the game package
    // and the staging directories, edited on the Storage tab's WHDLoad
    // sub-page. `args` has no row of its own; it is carried through so a
    // hand-written key survives a launcher save.
    whdload_game: Option<PathBuf>,
    whdload_kickstarts: Option<PathBuf>,
    whdload_library: Option<PathBuf>,
    whdload_args: Option<String>,
    /// The WHDLoad distribution and Soft-Kicker archives, when they are not
    /// the copies the release ships with.
    whdload_whd_package: Option<PathBuf>,
    whdload_skick_package: Option<PathBuf>,
    /// Which machine a package boots on.
    whdload_machine: crate::config::WhdloadMachine,
    /// Where the game database lives, and whether WHDLoad has a link of its
    /// own in the left-hand navigation. Both belong to the library, so a
    /// build without it carries them through a save untouched rather than
    /// editing or dropping them.
    /// Where the Library page keeps its scanned library and its downloads.
    /// Configuration-file only -- see `RawWhdload::library_db` -- so they
    /// are held here rather than being launcher fields with rows.
    whdload_library_db: Option<PathBuf>,
    whdload_library_cache: Option<PathBuf>,
    whdload_enabled: bool,
    /// A directory of packages, which the Library page lists.
    whdload_games: Option<PathBuf>,
    // Serial port. Carried in every build so a config's `[serial]` block
    // round-trips; only edited in the I/O Ports tab's Serial section, which a
    // `midi` build shows.
    serial_mode: SerialMode,
    midi_out: Option<String>,
    midi_in: Option<String>,
    /// TCP listen address for `mode = "tcp"`, typed into the Listen box the
    /// Serial section shows in that mode. `None` binds the default
    /// ([`crate::config::SERIAL_TCP_DEFAULT_LISTEN`]).
    serial_listen: Option<String>,
    /// Dial-out address for `mode = "tcp-connect"`, typed into the Connect
    /// box that mode shows. `None` there has nothing to dial, and the run
    /// says so rather than the launcher refusing the mode.
    serial_connect: Option<String>,
    /// `AT*T1`/`AT*T0` default at power-on for `mode = "modem"`, toggled by
    /// the Telnet row that mode shows.
    serial_telnet: bool,
    /// The Centronics parallel-port device (None/Printer/Sampler), edited in the
    /// I/O Ports tab's Parallel section.
    parallel_device: crate::config::ParallelDevice,
    /// The printer capture path, edited by the Output file row (shown when the
    /// Printer device is selected) and carried through from a hand-written
    /// `[parallel] output`.
    parallel_output: Option<PathBuf>,
    /// Sampler host capture device (`None` = system default) and its input gain,
    /// edited in the I/O Ports tab's Parallel section.
    sampler_input: Option<String>,
    sampler_gain_db: f32,
    /// The A2065 Ethernet board, edited in the I/O Ports tab's Ethernet
    /// section: `None` = not fitted, `Some(backend)` fits the board with that
    /// host backend (`NetConfig::None` = fitted but isolated).
    a2065_net: Option<NetConfig>,
    /// The bundled HostSocket bsdsocket.library board, edited in the same
    /// Ethernet section: `None` = not fitted, `Some(backend)` fits it.
    hostsocket_net: Option<NetConfig>,
    /// Whether the HostSocket board is in `net = "host"` mode (real host OS
    /// sockets via direct passthrough, `crate::hostsocket`'s own doc
    /// comment) -- not a `NetConfig` backend at all, so it can't live in
    /// `hostsocket_net` the way the other choices do; overrides it for
    /// display and saving when set (see `Config::hostsocket_transport`'s
    /// own comment for why this needs a field of its own).
    hostsocket_host_mode: bool,
    /// `[hostsocket]` keys the launcher does not edit (DNS resolver
    /// address/strategy, guest hostname, and the bridge-only interface
    /// address/gateway), carried through so saving a config keeps them.
    hostsocket_dns_server: Option<String>,
    hostsocket_hostname: Option<String>,
    hostsocket_address: Option<String>,
    hostsocket_gateway: Option<String>,
    hostsocket_resolver: Option<String>,
    /// The MacroSystem Toccata sound board, edited in the I/O Ports tab's
    /// Audio page (`[toccata] enabled`). No other options exist yet.
    toccata: bool,
    /// The freezer cartridge (`[cartridge] model`), edited in the System
    /// tab, and an image of the user's own (`[cartridge] rom`), carried
    /// across the round trip without a row.
    cartridge: Option<crate::cartridge::CartridgeModel>,
    cartridge_rom: Option<PathBuf>,
    /// The MHI virtual MPEG audio decoder board, edited on the same Audio
    /// page (`[mhi] enabled`) in an `mhi` build, the only build that can
    /// fit the board. Kept as an unconditional passthrough field even in a
    /// non-`mhi` build so loading and re-saving a config does not silently
    /// drop a `[mhi] enabled` set by some other build -- only the launcher
    /// row/toggle that edits it is feature-gated.
    mhi: bool,
    /// Currently visible host bridge adapters: stable identifier + label.
    bridge_interfaces: Vec<(String, String)>,
    /// Input device names for the sampler picker: filled when the screen opens
    /// and re-read each time the field is cycled, so a reconnected device
    /// appears.
    sampler_input_devices: Vec<String>,
    /// Host endpoints for the device pickers, read once when this setup is
    /// built so a fresh config screen sees currently-connected devices.
    #[cfg(feature = "midi")]
    midi_endpoints: crate::midi::MidiEndpoints,
    // A/V and emulation
    /// Host audio output selection: system default, a named device, or Disabled
    /// (no sound). Carried in every build so `[audio]` round-trips.
    audio_output: crate::audio::AudioOutput,
    /// Output device names for the picker: filled when the screen opens and
    /// re-read each time the field is cycled, so a reconnected device appears.
    audio_devices: Vec<String>,
    /// Stereo (hardware panning) or mono (L/R averaged).
    audio_channel_mode: ChannelMode,
    /// Stereo width, 0-100 (100 = full hardware panning).
    audio_stereo_separation: u8,
    /// Paula analogue filter override: Auto (guest-driven), On, or Off.
    audio_filter: AudioFilterMode,
    /// `[audio] stem_granularity`: the headless `--audio-stems-mode` default.
    /// No launcher row edits it (stem capture is a headless-only flag), but a
    /// loaded config's value must survive a Save, so it is carried through
    /// as a passthrough rather than silently dropped by `to_raw`.
    audio_stem_granularity: Option<Vec<crate::audio::mux::StemGranularity>>,
    overscan: Overscan,
    pixel_aspect: PixelAspect,
    /// How the canvas is scaled into the window ([display] scaling).
    scaling: DisplayScaling,
    /// Crop the presentation to the programmed display window
    /// ([display] autocrop).
    autocrop: bool,
    /// Motion-adaptive interlace weaving ([display] deinterlace).
    deinterlace: bool,
    phosphor: f32,
    /// Window shader pass ([display] shader).
    shader: ShaderMode,
    /// The user shader the loaded config named, kept even while another
    /// preset is selected so the picker can offer it again. A file the
    /// config never mentioned cannot be reached from here: the field takes
    /// a path, and this screen has no shader browser.
    shader_custom: Option<PathBuf>,
    /// Shader mix, 0.0 to 1.0 ([display] shader_strength).
    shader_strength: f32,
    /// Which monitor front frames the picture, if any ([display] bezel).
    bezel: BezelStyle,
    /// Folder of PNG stickers drawn onto the bezel ([display]
    /// bezel_stickers). No launcher row edits it -- like the custom
    /// shader, there is no file browser -- but it is carried so a
    /// configured folder survives the launcher's config round-trip.
    bezel_stickers: Option<PathBuf>,
    /// Performance overlay in the top-right ([display] perf_overlay).
    perf_overlay: bool,
    /// The MT-32's two ROM images, whether its front panel starts up, and
    /// how that panel's display is lit.
    mt32_control_rom: Option<PathBuf>,
    mt32_pcm_rom: Option<PathBuf>,
    mt32_panel: bool,
    mt32_lcd: Mt32Lcd,
    /// Coppersynth's soundfont and translation mode ([serial] coppersynth_*).
    csynth_soundfont: Option<PathBuf>,
    csynth_mt32_mode: Option<String>,
    csynth_panel: bool,
    /// How large the pop-up menu is drawn ([display] menu_scale).
    menu_scale: MenuScale,
    /// Screen tint ([display] tint).
    tint: Tint,
    /// Open fullscreen at start ([display] full_screen).
    start_fullscreen: bool,
    /// Show the status bar at start ([display] status_bar).
    show_status_bar: bool,
    floppy_sounds: bool,
    floppy_volume: u8,
    power_on: bool,
    auto_launch: bool,
    pacing_budget: PacingBudget,
    realtime_priority: bool,
    warp: WarpSpeed,
    /// The config screen has no control for run-ahead yet, but must preserve
    /// a value loaded from TOML or the CLI when it rebuilds RawConfig.
    run_ahead_frames: u8,
    /// No config-screen controls yet either; preserved across the round
    /// trip like run_ahead_frames (src/warpboot.rs).
    warp_boot: bool,
    warp_boot_idle: f64,
    warp_until: Option<f64>,
    /// Same again for the uaelib trap (crate::uaelib).
    uaelib: bool,
    /// Host-file commands have no launcher control, but a loaded opt-in must
    /// survive Save just like the uaelib enable itself.
    uaelib_files: bool,
    joystick_input_mode: JoystickInputMode,
    mouse_sensitivity: u8,
    mouse_capture: MouseCapture,
    port_devices: [PortDevice; 2],
    // Extra Zorro boards (metadata path + plugin config schema/overrides)
    zorro_boards: Vec<ZorroBoardSetup>,
    /// The Paths page: the configuration's `[paths]` section, saved with
    /// the rest of it. Empty until somebody moves something, so a
    /// configuration that never mentions directories stays as portable as
    /// it was -- and one that does still starts on a machine that has
    /// never seen them, because what cannot be reached is dropped when the
    /// configuration is adopted.
    paths: crate::pathconf::Paths,
}

impl Default for MachineSetup {
    fn default() -> Self {
        // The empty raw config is always valid (the built-in defaults).
        Self::from_raw(&RawConfig::default()).expect("default config is valid")
    }
}

impl MachineSetup {
    /// Re-read the host MIDI endpoints for the device pickers.
    #[cfg(feature = "midi")]
    pub fn refresh_midi_endpoints(&mut self) {
        self.midi_endpoints = crate::midi::enumerate();
    }

    /// Re-read the host audio output devices for the "Audio output" picker.
    pub fn refresh_audio_devices(&mut self) {
        self.audio_devices = crate::audio::picker_output_devices();
    }

    /// Re-read the host audio input devices for the sampler "Audio input" picker.
    pub fn refresh_sampler_inputs(&mut self) {
        self.sampler_input_devices = crate::sampler::picker_input_devices();
    }

    /// The selected serial mode and parallel device, so the panel can pick the
    /// dynamic Serial/Parallel row sets (see [`rows`]).
    /// Whether the MIDI output is pointed at the built-in MT-32, which is
    /// what puts its ROM and panel rows on the I/O Ports tab.
    pub fn midi_out_is_mt32(&self) -> bool {
        crate::config::midi_out_is_mt32(self.midi_out.as_deref())
    }

    pub fn midi_out_is_csynth(&self) -> bool {
        crate::config::midi_out_is_csynth(self.midi_out.as_deref())
    }

    /// Whether this configuration asks the launcher to run it the moment
    /// it is opened -- `[emulation] auto_launch`. The command line never
    /// consults it: a machine given on the command line was never going to
    /// see the configuration screen this skips.
    pub fn auto_launch(&self) -> bool {
        self.auto_launch
    }

    pub fn serial_mode(&self) -> SerialMode {
        self.serial_mode
    }

    pub fn parallel_device(&self) -> ParallelDevice {
        self.parallel_device
    }

    /// The address one of the serial TCP boxes holds, or `None` while it is
    /// unset (the run then dials nothing and binds the default).
    #[cfg(feature = "midi")]
    pub fn serial_addr(&self, field: LauncherField) -> Option<&str> {
        match field {
            F::SerialConnect => self.serial_connect.as_deref(),
            F::SerialListen => self.serial_listen.as_deref(),
            _ => None,
        }
    }

    /// Store what was typed into one of the serial TCP boxes. `None` clears
    /// the key from a saved config rather than writing an empty address.
    #[cfg(feature = "midi")]
    pub fn set_serial_addr(&mut self, field: LauncherField, addr: Option<String>) {
        match field {
            F::SerialConnect => self.serial_connect = addr,
            F::SerialListen => self.serial_listen = addr,
            _ => {}
        }
    }

    /// The halves of a serial address, either absent while untouched.
    #[cfg(feature = "midi")]
    pub fn serial_addr_parts(&self, field: LauncherField) -> (Option<String>, Option<u16>) {
        self.serial_addr(field)
            .map(split_host_port)
            .unwrap_or((None, None))
    }

    /// Replace one half of a serial address, keeping the other. Emptying
    /// both empties the whole (the key leaves a saved config); a half on
    /// its own is completed with the defaults the run would use.
    #[cfg(feature = "midi")]
    pub fn set_serial_addr_part(
        &mut self,
        field: LauncherField,
        host: Option<Option<String>>,
        port: Option<Option<u16>>,
    ) {
        let (cur_host, cur_port) = self.serial_addr_parts(field);
        let host = host.unwrap_or(cur_host);
        let port = port.unwrap_or(cur_port);
        self.set_serial_addr(field, join_host_port(host.as_deref(), port));
    }

    /// Whether the selected Ethernet backend carries traffic on the host's
    /// schedule rather than the emulated clock, breaking byte-identical
    /// replay (the I/O Ports tab shows a warning). The loopback backend
    /// echoes frames deterministically and an isolated or absent NIC never
    /// sees traffic, so NAT and a direct adapter bridge qualify -- as does
    /// the HostSocket board's own `Host` mode, real host OS sockets on the
    /// host's schedule same as NAT/bridge, just without an intervening
    /// `NetConfig` backend to match on.
    pub fn ethernet_breaks_determinism(&self) -> bool {
        self.hostsocket_host_mode
            || [&self.a2065_net, &self.hostsocket_net].iter().any(|net| {
                matches!(
                    net.as_ref(),
                    Some(NetConfig::Nat) if crate::net::NAT_AVAILABLE
                ) || matches!(
                    net.as_ref(),
                    Some(NetConfig::Bridge { .. }) if crate::net::BRIDGE_AVAILABLE
                )
            })
    }

    /// Re-read every host device list (MIDI endpoints + audio outputs + sampler
    /// inputs) for the pickers. Call after (re)building the setup -- e.g. loading
    /// a config or resetting to defaults -- so the pickers show what is connected
    /// now instead of an empty list that can only land on "Default"/"None".
    pub fn refresh_host_devices(&mut self) {
        #[cfg(feature = "midi")]
        self.refresh_midi_endpoints();
        self.refresh_audio_devices();
        self.refresh_sampler_inputs();
        self.refresh_bridge_interfaces();
    }

    fn refresh_bridge_interfaces(&mut self) {
        self.bridge_interfaces.clear();
        #[cfg(all(feature = "net-bridge", not(target_arch = "wasm32")))]
        match crate::net::bridge::list_interfaces() {
            Ok(interfaces) => {
                self.bridge_interfaces = interfaces
                    .into_iter()
                    .filter(|interface| !interface.loopback)
                    .map(|interface| (interface.name.clone(), interface.label()))
                    .collect();
            }
            Err(error) => log::warn!("launcher: cannot enumerate bridge adapters: {error:#}"),
        }
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
        self.rtg = base.rtg;
        self.rtg_vram_bytes = base.rtg_vram_bytes;
        self.cpu = base.cpu;
        self.fpu = base.fpu;
        self.clock_mhz = base.cpu_clock_mhz;
        self.icache = base.cpu_icache;
        self.dcache = base.cpu_dcache;
        self.jit = base.cpu_jit;
        self.chip_ram = base.chip_ram_bytes;
        self.fast_ram = base.fast_ram_bytes;
        self.slow_ram = base.slow_ram_bytes;
        self.mb_ram = base.mb_ram_bytes;
        self.accel_ram = base.accel_ram_bytes;
        self.z3_ram = base.z3_ram_bytes;
        self.overscan = base.overscan;
        self.pixel_aspect = base.pixel_aspect;
        self.scaling = base.scaling;
        self.autocrop = base.autocrop;
        self.deinterlace = base.deinterlace;
        self.phosphor = base.phosphor;
        // The remembered user-shader path survives: it came from the config
        // file, not from the profile, and the picker needs it to offer
        // "Custom" again.
        self.shader = base.shader.clone();
        self.shader_strength = base.shader_strength;
        self.bezel = base.bezel;
        // The sticker folder survives like the shader path above: it names
        // a folder of the user's, not anything of the profile's.
        self.perf_overlay = base.perf_overlay;
        self.tint = base.tint;
        self.menu_scale = base.menu_scale;
        self.mt32_control_rom = base.serial.mt32_control_rom.clone();
        self.mt32_pcm_rom = base.serial.mt32_pcm_rom.clone();
        self.mt32_panel = base.serial.mt32_panel;
        self.mt32_lcd = base.serial.mt32_lcd;
        self.csynth_soundfont = base.serial.coppersynth_soundfont.clone();
        self.csynth_mt32_mode = base.serial.coppersynth_mt32_mode.clone();
        self.csynth_panel = base.serial.coppersynth_panel;
        self.start_fullscreen = base.full_screen;
        self.show_status_bar = base.status_bar;
        self.floppy_sounds = base.audio.floppy_sounds;
        self.floppy_volume = base.audio.floppy_sounds_volume;
        self.power_on = base.emulation.power_on;
        self.auto_launch = base.emulation.auto_launch;
        self.pacing_budget = base.emulation.pacing_budget;
        self.realtime_priority = base.emulation.realtime_priority;
        self.warp = base.emulation.warp_speed;
        self.run_ahead_frames = base.emulation.run_ahead_frames;
        self.uaelib = base.emulation.uaelib;
        self.uaelib_files = base.emulation.uaelib_files;
        self.cartridge = base.cartridge.model;
        self.cartridge_rom = None;
        self.joystick_input_mode = base.joystick_input_mode;
        self.mouse_sensitivity = base.mouse_sensitivity;
        self.mouse_capture = base.mouse_capture;
        self.port_devices = base.port_devices;
        let profile_drives = connected_floppy_bays(&base.floppy_connected);
        self.floppy_drives = profile_drives.max(self.occupied_floppy_bays());
        if !self.has_ide() {
            self.ide_master = None;
            self.ide_master_name = None;
            self.ide_master_bootpri = None;
            self.ide_slave = None;
            self.ide_slave_name = None;
            self.ide_slave_bootpri = None;
        }
        if !self.has_cd() {
            self.cd_image = None;
            self.cd_insert_delay = 0.0;
        }
        if model != Some(MachineModel::Cd32) {
            self.cd32_nvram = None;
            self.fmv_rom = None;
            self.fmv_rom_disabled = false;
        }
        // The motherboard SCSI leaves with the motherboard; the drives stay and
        // land on the default Zorro board instead.
        if !self.has_sdmac() && self.scsi_controller == Some(ScsiController::A3000) {
            self.scsi_controller = Some(ScsiController::A2091);
        }
        // Last, once the ports this machine has are settled: a real disk on
        // one it does not have goes the same way an image on it does.
        self.drop_unreachable_host_disks();
    }

    fn has_gayle(&self) -> bool {
        matches!(self.model, Some(MachineModel::A600 | MachineModel::A1200))
    }

    fn has_sdmac(&self) -> bool {
        self.model == Some(MachineModel::A3000)
    }

    /// Whether a SCSI controller is fitted, which is what a SCSI unit needs
    /// to exist at all. The A3000's choice needs the motherboard silicon
    /// behind it; a Zorro board needs only to have been chosen.
    pub fn has_scsi_controller(&self) -> bool {
        match self.scsi_controller {
            Some(ScsiController::A3000) => self.has_sdmac(),
            Some(_) => true,
            None => false,
        }
    }

    /// Machines with an IDE port: Gayle's, and the A4000's at $DD2020.
    pub fn has_ide(&self) -> bool {
        self.has_gayle() || self.model == Some(MachineModel::A4000)
    }

    fn has_cd(&self) -> bool {
        matches!(self.model, Some(MachineModel::Cdtv | MachineModel::Cd32))
    }

    /// Whether a field is applicable to the current machine (greyed otherwise).
    pub fn applies(&self, field: LauncherField) -> bool {
        self.disabled_reason(field).is_none()
    }

    /// Whether a row is hidden entirely (not just greyed). The Floppy tab only
    /// shows a drive's rows once it is wired in, so unused drives simply do not
    /// appear rather than sitting greyed -- there is never a reason to touch a
    /// drive that is not enabled.
    pub fn row_hidden(&self, field: LauncherField) -> bool {
        use LauncherField as F;
        match field {
            F::Df0Image | F::Df0WriteProtect => self.floppy_drives < 1,
            F::Df1Image | F::Df1WriteProtect => self.floppy_drives < 2,
            F::Df2Image | F::Df2WriteProtect => self.floppy_drives < 3,
            F::Df3Image | F::Df3WriteProtect => self.floppy_drives < 4,
            // The word only exists while the fill is Fixed; the other
            // fills have nothing to edit, so the row goes rather than
            // sitting greyed under a setting most people never touch.
            F::RamPattern => !matches!(self.ram_init, RamInit::Pattern { .. }),
            F::EthernetInterface => {
                !matches!(self.a2065_net.as_ref(), Some(NetConfig::Bridge { .. }))
            }
            F::HostSocketInterface => {
                !matches!(self.hostsocket_net.as_ref(), Some(NetConfig::Bridge { .. }))
            }
            // No controller, no SCSI: the ROM and unit rows go rather than
            // stand greyed under a setting most machines leave at None. They
            // return the moment a controller is chosen.
            F::ScsiRom
            | F::ScsiRomOdd
            | F::ScsiUnit0
            | F::ScsiUnit1
            | F::ScsiUnit2
            | F::ScsiUnit3
            | F::ScsiUnit4
            | F::ScsiUnit5
            | F::ScsiUnit6 => self.scsi_controller.is_none(),
            // A controller is not a disk: only the units carrying one have a
            // place in the boot order, so the empty six or seven go rather
            // than standing in a column saying so.
            F::ScsiUnit0Boot
            | F::ScsiUnit1Boot
            | F::ScsiUnit2Boot
            | F::ScsiUnit3Boot
            | F::ScsiUnit4Boot
            | F::ScsiUnit5Boot
            | F::ScsiUnit6Boot => {
                self.scsi_controller.is_none()
                    || Self::boot_field_drive(field)
                        .and_then(|drive| self.drive_holds(drive))
                        .is_none()
            }
            // Read on the Boot Priority page beside the SCSI units, and on
            // the same terms: a board is not a disk, so only a slot the
            // fitted board has *and* which carries one takes a place in the
            // boot order. The Lide page keeps the drives themselves.
            F::LideDrive0Boot | F::LideDrive1Boot | F::LideDrive2Boot | F::LideDrive3Boot => {
                lide_drive_index(field).is_none_or(|i| {
                    self.lide_board.is_none_or(|b| b.max_drives() <= i)
                        || Self::boot_field_drive(field)
                            .and_then(|drive| self.drive_holds(drive))
                            .is_none()
                })
            }
            // copperhf units are always fitted (there is no board to lack),
            // so the only reason a unit's boot row goes is an empty slot --
            // the same terms as the SCSI units above.
            F::CopperhfUnit0Boot
            | F::CopperhfUnit1Boot
            | F::CopperhfUnit2Boot
            | F::CopperhfUnit3Boot
            | F::CopperhfUnit4Boot
            | F::CopperhfUnit5Boot
            | F::CopperhfUnit6Boot => Self::boot_field_drive(field)
                .and_then(|drive| self.drive_holds(drive))
                .is_none(),
            // Nothing to configure without a board fitted.
            F::LideRom => self.lide_board.is_none(),
            // AT-Bus 2008 has no flash banking.
            F::LideRomBank2 => self
                .lide_board
                .is_none_or(|b| b == LidePersonality::AtBus2008),
            // Only a slot the fitted board does not have stays hidden
            // (RIDE/AT-Bus 2008 have one channel, RIPPLE two). Every slot
            // the board *does* have is always editable, the same way
            // `[ide]`'s master/slave and `[scsi]`'s units are: each is its
            // own `driveN` key, so filling one without the one before it is
            // representable.
            F::LideDrive0 | F::LideDrive1 | F::LideDrive2 | F::LideDrive3 => {
                lide_drive_index(field)
                    .is_some_and(|i| self.lide_board.is_none_or(|b| b.max_drives() <= i))
            }
            _ => false,
        }
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
            // The 68000/68010 shared-bus float model needs the precise
            // core, so the JIT never engages there (see cpu.rs).
            F::Jit => reason(
                !matches!(self.cpu, CpuModel::M68000 | CpuModel::M68010),
                "needs 68020+",
            ),
            F::Z3Ram => reason(cpu_is_32bit(self.cpu), "needs 32-bit CPU"),
            // The CPU-slot space at $08000000 is beyond a 24-bit bus too.
            F::AccelRam => reason(cpu_is_32bit(self.cpu), "needs 32-bit CPU"),
            // The Zorro II cards (Picasso II/II+, Graffity [Zorro II]) remain
            // available on a 24-bit CPU. The stepper's choice list omits the
            // Zorro III-only cards (Graffity [Zorro III], Z3660) in that case.
            F::Rtg => None,
            // Motherboard fast RAM hangs off Ramsey, which only the big-box
            // profiles fit, and its bank ends beyond a 24-bit address bus.
            F::MbRam => {
                let big_box = matches!(self.model, Some(MachineModel::A3000 | MachineModel::A4000));
                reason(
                    big_box && cpu_is_32bit(self.cpu),
                    if big_box {
                        "needs 32-bit CPU"
                    } else {
                        "needs A3000/A4000"
                    },
                )
            }
            F::IdeMaster | F::IdeSlave => reason(self.has_ide(), "needs A600/A1200/A4000 or Lide"),
            // The ROM and drives belong to the fitted controller, and with
            // none the rows are hidden outright (`row_hidden`), so only a
            // fitted-but-unsuitable controller is ever explained here: the
            // A3000's motherboard SCSI has no ROM of its own, and rom_odd
            // is an A2091 split-EPROM option only.
            F::ScsiRom => reason(
                self.scsi_controller
                    .is_some_and(ScsiController::is_zorro_board),
                "Zorro boards only",
            ),
            F::ScsiRomOdd => reason(
                self.scsi_controller == Some(ScsiController::A2091),
                "A2091 only",
            ),
            F::CdImage | F::CdInsertDelay => {
                reason(self.has_cd(), "needs CDTV/CD32 (or SCSI/IDE/Lide)")
            }
            F::Cd32Nvram => reason(self.model == Some(MachineModel::Cd32), "CD32 only"),
            F::FmvRom => reason(self.model == Some(MachineModel::Cd32), "CD32 only"),
            F::Df0Image | F::Df0WriteProtect => reason(self.floppy_drives >= 1, "drive off"),
            F::Df1Image | F::Df1WriteProtect => reason(self.floppy_drives >= 2, "drive off"),
            F::Df2Image | F::Df2WriteProtect => reason(self.floppy_drives >= 3, "drive off"),
            F::Df3Image | F::Df3WriteProtect => reason(self.floppy_drives >= 4, "drive off"),
            // Drive speed shapes how fast a track is served from an image; a
            // real drive's data rate is the disk's own. With every fitted bay
            // physical there is nothing for it to act on.
            F::FloppySpeed => {
                let any_image =
                    (0..self.floppy_drives as usize).any(|i| self.df_bridge[i].is_none());
                reason(any_image, "no image drives")
            }
            // Shader strength only feeds the shader pass, which does not run when
            // the shader is off.
            F::ShaderStrength => reason(self.shader != ShaderMode::None, "Disabled"),
            // A boot priority or read-only flag is meaningless without a
            // directory to mount.
            F::Filesys0Boot | F::Filesys1Boot | F::Filesys2Boot | F::Filesys3Boot => {
                let (slot, _) = filesys_slot(field).expect("boot field");
                reason(self.filesys_dirs[slot].is_some(), "no directory")
            }
            // Boot priority applies only to a hard-disk image: greyed for an
            // empty slot, or a CD image (which boots by its own rules).
            F::IdeMasterBoot
            | F::IdeSlaveBoot
            | F::ScsiUnit0Boot
            | F::ScsiUnit1Boot
            | F::ScsiUnit2Boot
            | F::ScsiUnit3Boot
            | F::ScsiUnit4Boot
            | F::ScsiUnit5Boot
            | F::ScsiUnit6Boot
            | F::LideDrive0Boot
            | F::LideDrive1Boot
            | F::LideDrive2Boot
            | F::LideDrive3Boot
            | F::CopperhfUnit0Boot
            | F::CopperhfUnit1Boot
            | F::CopperhfUnit2Boot
            | F::CopperhfUnit3Boot
            | F::CopperhfUnit4Boot
            | F::CopperhfUnit5Boot
            | F::CopperhfUnit6Boot => {
                let drive = Self::boot_field_drive(field).expect("boot field");
                match self.drive_holds(drive) {
                    None => Some("No drive"),
                    Some(DriveContents::Image(p)) if crate::config::is_cd_image_path(p) => {
                        Some("CD-ROM")
                    }
                    // A real disk's partitions carry their own de_BootPri, on
                    // the disk, and nothing here overrides that. Naming what
                    // the slot holds says as much and reads plainer.
                    Some(DriveContents::HostDisk) => Some("Host Disk"),
                    Some(DriveContents::Image(_)) => None,
                }
            }
            F::Filesys0ReadOnly
            | F::Filesys1ReadOnly
            | F::Filesys2ReadOnly
            | F::Filesys3ReadOnly => {
                let slot = filesys_readonly_slot(field).expect("readonly field");
                reason(self.filesys_dirs[slot].is_some(), "no directory")
            }
            // The MIDI endpoint and sampler input/gain rows are hidden entirely
            // when inactive (see `rows`), so they never need a greyed state.
            // Channel mode and separation shape the output, so they do nothing
            // once audio is disabled; separation also does nothing in mono.
            // The bridge page follows what is there. A loaded config can
            // pull the bay out from under the page: every row greys, the
            // Interface one included. With the bay bridged but no interface
            // attached or selected, only the Interface row stays live -- the
            // rest describe hardware that is not present.
            #[cfg(feature = "fluxbridge")]
            F::BridgeDevice => reason(self.bridge_edit().is_some(), "No drive"),
            #[cfg(feature = "fluxbridge")]
            F::BridgeCable | F::BridgeDensity | F::BridgeReadMode | F::BridgeReplaySpeed
                if self.bridge_edit().is_none()
                    || self.df_bridge_none[self.bridge_edit_drive]
                    || self.bridge_status == BridgeStatus::NoInterface =>
            {
                Some("no interface")
            }
            // The port row stays live while there is a port to pick, even
            // with nothing recognised as attached: an interface on a serial
            // chip the scan does not name is selected by hand. It greys with
            // the interface set to None, and with nothing to pick (a list of
            // just "Automatic").
            #[cfg(feature = "fluxbridge")]
            F::BridgePort => {
                if self.bridge_edit().is_none() || self.df_bridge_none[self.bridge_edit_drive] {
                    Some("no interface")
                } else if self.bridge_ports.len() <= 1 {
                    Some("no ports")
                } else {
                    reason(
                        self.bridge_driver_supports(crate::fluxbridge::config_option::COM_PORT),
                        "not on this interface",
                    )
                }
            }
            #[cfg(feature = "fluxbridge")]
            F::BridgeCable => reason(
                self.bridge_driver_supports(crate::fluxbridge::config_option::DRIVE_AB_CABLE)
                    || self
                        .bridge_driver_supports(crate::fluxbridge::config_option::SUPPORTS_SHUGART),
                "not on this interface",
            ),
            F::AudioChannelMode => reason(self.audio_output.is_enabled(), "off"),
            F::AudioFilter => reason(self.audio_output.is_enabled(), "off"),
            F::AudioStereoSeparation => {
                if !self.audio_output.is_enabled() {
                    Some("off")
                } else {
                    reason(self.audio_channel_mode != ChannelMode::Mono, "mono")
                }
            }
            // Neither mouse row does anything unless a port holds a mouse.
            F::MouseSensitivity | F::MouseCapture => {
                reason(self.port_devices.iter().any(|d| d.is_mouse()), "No mouse")
            }
            _ => None,
        }
    }

    /// The current boolean of a toggle field.
    /// Whether WHDLoad has a link of its own in the left-hand strip.
    pub fn whdload_enabled(&self) -> bool {
        self.whdload_enabled
    }

    /// The bundled-ROM label for the selected SCSI controller.
    pub fn scsi_bundled_rom_label(&self) -> Option<&'static str> {
        match self.scsi_controller {
            Some(ScsiController::A2091) => Some("(bundled open A2091 ROM)"),
            Some(ScsiController::A4091) => Some("(bundled A4091 ROM)"),
            _ => None,
        }
    }

    /// Whether the CD32 FMV cartridge was explicitly removed rather than
    /// left to inherit the bundled open ROM.
    pub fn fmv_rom_disabled(&self) -> bool {
        self.fmv_rom_disabled
    }

    /// Switch the CD32 FMV row between the bundled module and an empty slot.
    /// A named replacement is removed along with the module; the next press
    /// restores the bundled default.
    pub fn toggle_fmv_module(&mut self) {
        self.fmv_rom = None;
        self.fmv_rom_disabled = !self.fmv_rom_disabled;
    }

    /// The current path of a path field, if any.
    pub fn path(&self, field: LauncherField) -> Option<&Path> {
        match field {
            F::Rom => self.rom.as_deref(),
            F::Mt32ControlRom => self.mt32_control_rom.as_deref(),
            #[cfg(feature = "coppersynth")]
            F::CsynthSoundfont => self.csynth_soundfont.as_deref(),
            F::Mt32PcmRom => self.mt32_pcm_rom.as_deref(),
            F::ExtendedRom => self.extended_rom.as_deref(),
            F::FmvRom => self.fmv_rom.as_deref(),
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
            F::CopperhfUnit0 => self.copperhf_units[0].as_deref(),
            F::CopperhfUnit1 => self.copperhf_units[1].as_deref(),
            F::CopperhfUnit2 => self.copperhf_units[2].as_deref(),
            F::CopperhfUnit3 => self.copperhf_units[3].as_deref(),
            F::CopperhfUnit4 => self.copperhf_units[4].as_deref(),
            F::CopperhfUnit5 => self.copperhf_units[5].as_deref(),
            F::CopperhfUnit6 => self.copperhf_units[6].as_deref(),
            F::LideRom => self.lide_rom.as_deref(),
            F::LideRomBank2 => self.lide_rom_bank2.as_deref(),
            F::LideDrive0 => self.lide_drives[0].as_deref(),
            F::LideDrive1 => self.lide_drives[1].as_deref(),
            F::LideDrive2 => self.lide_drives[2].as_deref(),
            F::LideDrive3 => self.lide_drives[3].as_deref(),
            F::Filesys0Dir => self.filesys_dirs[0].as_deref(),
            F::Filesys1Dir => self.filesys_dirs[1].as_deref(),
            F::Filesys2Dir => self.filesys_dirs[2].as_deref(),
            F::Filesys3Dir => self.filesys_dirs[3].as_deref(),
            F::CdImage => self.cd_image.as_deref(),
            F::Cd32Nvram => self.cd32_nvram.as_deref(),
            F::ParallelOutput => self.parallel_output.as_deref(),
            F::WhdloadGame => self.whdload_game.as_deref(),
            F::WhdloadKickstarts => self.whdload_kickstarts.as_deref(),
            F::WhdloadLibrary => self.whdload_library.as_deref(),
            F::WhdloadWhdPackage => self.whdload_whd_package.as_deref(),
            F::WhdloadSkickPackage => self.whdload_skick_package.as_deref(),
            F::WhdloadGames => self.whdload_games.as_deref(),
            // What the entry says, not where it resolves to: this is the
            // value being edited. `paths_resolved` answers the other
            // question, and the row shows that one.
            _ => self.paths_entry_ref(field).and_then(Option::as_deref),
        }
    }

    /// The `[paths]` entry a Paths row edits.
    ///
    /// `None` for anything that is not a Paths row, which is what keeps the
    /// twelve directories out of every other path accessor's way.
    fn paths_entry(
        paths: &mut crate::pathconf::Paths,
        field: LauncherField,
    ) -> Option<&mut Option<PathBuf>> {
        let p = paths;
        Some(match field {
            F::PathsBase => &mut p.base,
            F::PathsStates => &mut p.states,
            F::PathsScreenshots => &mut p.screenshots,
            F::PathsRecordings => &mut p.recordings,
            F::PathsNvram => &mut p.nvram,
            F::PathsTraces => &mut p.traces,
            F::PathsConfigs => &mut p.configs,
            F::PathsRoms => &mut p.roms,
            F::PathsFloppies => &mut p.floppies,
            F::PathsHarddrives => &mut p.harddrives,
            F::PathsCds => &mut p.cds,
            _ => return None,
        })
    }

    /// The same entry, to read. Written out twice because a borrow cannot
    /// be handed back from a copy; the test below walks every row through
    /// both, so the pair cannot drift apart unnoticed.
    fn paths_entry_ref(&self, field: LauncherField) -> Option<&Option<PathBuf>> {
        let p = &self.paths;
        Some(match field {
            F::PathsBase => &p.base,
            F::PathsStates => &p.states,
            F::PathsScreenshots => &p.screenshots,
            F::PathsRecordings => &p.recordings,
            F::PathsNvram => &p.nvram,
            F::PathsTraces => &p.traces,
            F::PathsConfigs => &p.configs,
            F::PathsRoms => &p.roms,
            F::PathsFloppies => &p.floppies,
            F::PathsHarddrives => &p.harddrives,
            F::PathsCds => &p.cds,
            _ => return None,
        })
    }

    /// Where a Paths row points now, resolved -- which for an unset row is
    /// the directory it inherits. The page is there to say where things
    /// actually go, so an inherited row still names a directory rather than
    /// going blank and leaving the answer somewhere else.
    pub fn paths_resolved(&self, field: LauncherField) -> Option<PathBuf> {
        let host = crate::paths::config_dir()?;
        let p = &self.paths;
        Some(match field {
            F::PathsBase => p.base_dir(&host),
            F::PathsStates => p.states_dir(&host),
            F::PathsScreenshots => p.screenshots_dir(&host),
            F::PathsRecordings => p.recordings_dir(&host),
            F::PathsNvram => p.nvram_dir(&host),
            F::PathsTraces => p.traces_dir(&host),
            F::PathsConfigs => p.configs_dir(&host),
            F::PathsRoms => p.roms_dir(&host),
            F::PathsFloppies => p.floppies_dir(&host),
            F::PathsHarddrives => p.harddrives_dir(&host),
            F::PathsCds => p.cds_dir(&host),
            _ => return None,
        })
    }

    /// Whether a Paths row was set rather than inherited.
    pub fn paths_is_set(&self, field: LauncherField) -> bool {
        self.paths_entry_ref(field).is_some_and(Option::is_some)
    }

    /// What a path row shows when the whole path is the point rather than
    /// the file at the end of it: the printer's capture file, and every row
    /// on the Paths page. `None` leaves the row to its usual file name.
    pub fn full_path_label(&self, field: LauncherField) -> Option<String> {
        if field == F::ParallelOutput {
            return Some(match self.path(field) {
                Some(path) => path.display().to_string(),
                None => "(none)".to_string(),
            });
        }
        if !field.is_paths_field() {
            return None;
        }
        // An inheriting row says so and stops there. Where a default goes
        // is Copperline's business until somebody makes it theirs, and a
        // page of eleven paths nobody chose is a page nobody reads.
        //
        // The base is the exception: it is the root the rest of them hang
        // off, so it names its directory whether or not it was set. With
        // it inheriting too, the page would say where nothing goes.
        if !self.paths_is_set(field) && field != F::PathsBase {
            return Some("(default)".to_string());
        }
        let resolved = self
            .paths_resolved(field)
            .or_else(|| self.path(field).map(Path::to_path_buf));
        Some(match resolved {
            Some(dir) => dir.display().to_string(),
            // No host directory at all, so nothing is resolvable and the
            // defaults land beside wherever Copperline was started.
            None => "(default)".to_string(),
        })
    }

    /// Put the edited directories in force.
    ///
    /// They are saved with the configuration like everything else on these
    /// pages -- `[paths]` is a section of it, not a file of its own -- but
    /// they take effect the moment they are changed, so a screenshot taken
    /// after moving the row lands where the row now says rather than where
    /// it said when the launcher opened.
    fn apply_paths(&self) {
        crate::paths::adopt(self.paths.clone());
    }

    /// Store a directory a Paths row was pointed at.
    ///
    /// A directory under the base is stored relative to it, so a tree that
    /// was set up as one piece still moves as one piece: point the base at
    /// a memory stick and everything under it follows, which is the whole
    /// reason the entries resolve the way they do. The base itself is
    /// always absolute -- a relative base is taken from the host-data
    /// directory, and quietly re-anchoring it there is not what somebody
    /// picking a folder meant.
    fn set_paths_dir(&mut self, field: LauncherField, dir: PathBuf) {
        let relative = (field != F::PathsBase)
            .then(|| {
                let base = crate::paths::config_dir()?;
                let base = self.paths.base_dir(&base);
                dir.strip_prefix(base).ok().map(Path::to_path_buf)
            })
            .flatten();
        if let Some(entry) = Self::paths_entry(&mut self.paths, field) {
            *entry = Some(relative.unwrap_or(dir));
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
                | F::CopperhfUnit0
                | F::CopperhfUnit1
                | F::CopperhfUnit2
                | F::CopperhfUnit3
                | F::CopperhfUnit4
                | F::CopperhfUnit5
                | F::CopperhfUnit6
                | F::LideDrive0
                | F::LideDrive1
                | F::LideDrive2
                | F::LideDrive3
                | F::Filesys0Dir
                | F::Filesys1Dir
                | F::Filesys2Dir
                | F::Filesys3Dir
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
            F::CopperhfUnit0 => &self.copperhf_unit_names[0],
            F::CopperhfUnit1 => &self.copperhf_unit_names[1],
            F::CopperhfUnit2 => &self.copperhf_unit_names[2],
            F::CopperhfUnit3 => &self.copperhf_unit_names[3],
            F::CopperhfUnit4 => &self.copperhf_unit_names[4],
            F::CopperhfUnit5 => &self.copperhf_unit_names[5],
            F::CopperhfUnit6 => &self.copperhf_unit_names[6],
            F::LideDrive0 => &self.lide_drive_names[0],
            F::LideDrive1 => &self.lide_drive_names[1],
            F::LideDrive2 => &self.lide_drive_names[2],
            F::LideDrive3 => &self.lide_drive_names[3],
            F::Filesys0Dir => &self.filesys_names[0],
            F::Filesys1Dir => &self.filesys_names[1],
            F::Filesys2Dir => &self.filesys_names[2],
            F::Filesys3Dir => &self.filesys_names[3],
            _ => return None,
        };
        name.as_deref()
    }

    /// The in-memory volume's filesystem for a directory-mount drive field
    /// (FFS by default). Meaningless -- but still tracked, so a toggle made
    /// before Browse is not lost -- for a field whose path is not currently
    /// a directory; `to_raw` only emits it once the path actually is one.
    pub fn drive_filesystem(&self, field: LauncherField) -> crate::diskimage::FileSystem {
        match field {
            F::IdeMaster => self.ide_master_fs,
            F::IdeSlave => self.ide_slave_fs,
            F::ScsiUnit0 => self.scsi_unit_fs[0],
            F::ScsiUnit1 => self.scsi_unit_fs[1],
            F::ScsiUnit2 => self.scsi_unit_fs[2],
            F::ScsiUnit3 => self.scsi_unit_fs[3],
            F::ScsiUnit4 => self.scsi_unit_fs[4],
            F::ScsiUnit5 => self.scsi_unit_fs[5],
            F::ScsiUnit6 => self.scsi_unit_fs[6],
            F::CopperhfUnit0 => self.copperhf_unit_fs[0],
            F::CopperhfUnit1 => self.copperhf_unit_fs[1],
            F::CopperhfUnit2 => self.copperhf_unit_fs[2],
            F::CopperhfUnit3 => self.copperhf_unit_fs[3],
            F::CopperhfUnit4 => self.copperhf_unit_fs[4],
            F::CopperhfUnit5 => self.copperhf_unit_fs[5],
            F::CopperhfUnit6 => self.copperhf_unit_fs[6],
            F::LideDrive0 => self.lide_drive_fs[0],
            F::LideDrive1 => self.lide_drive_fs[1],
            F::LideDrive2 => self.lide_drive_fs[2],
            F::LideDrive3 => self.lide_drive_fs[3],
            _ => crate::diskimage::FileSystem::FFS,
        }
    }

    /// Whether a disk-backed drive field's current path is a host directory
    /// (as opposed to an image file) -- sampled when the path was last set
    /// or loaded, not by statting it here: see `ide_master_is_dir`'s doc
    /// comment. `false` for any field that is not one of the drive fields
    /// this applies to, matching `drive_filesystem`'s fallback above.
    pub fn drive_is_directory(&self, field: LauncherField) -> bool {
        match field {
            F::IdeMaster => self.ide_master_is_dir,
            F::IdeSlave => self.ide_slave_is_dir,
            F::ScsiUnit0 => self.scsi_unit_is_dir[0],
            F::ScsiUnit1 => self.scsi_unit_is_dir[1],
            F::ScsiUnit2 => self.scsi_unit_is_dir[2],
            F::ScsiUnit3 => self.scsi_unit_is_dir[3],
            F::ScsiUnit4 => self.scsi_unit_is_dir[4],
            F::ScsiUnit5 => self.scsi_unit_is_dir[5],
            F::ScsiUnit6 => self.scsi_unit_is_dir[6],
            F::CopperhfUnit0 => self.copperhf_unit_is_dir[0],
            F::CopperhfUnit1 => self.copperhf_unit_is_dir[1],
            F::CopperhfUnit2 => self.copperhf_unit_is_dir[2],
            F::CopperhfUnit3 => self.copperhf_unit_is_dir[3],
            F::CopperhfUnit4 => self.copperhf_unit_is_dir[4],
            F::CopperhfUnit5 => self.copperhf_unit_is_dir[5],
            F::CopperhfUnit6 => self.copperhf_unit_is_dir[6],
            F::LideDrive0 => self.lide_drive_is_dir[0],
            F::LideDrive1 => self.lide_drive_is_dir[1],
            F::LideDrive2 => self.lide_drive_is_dir[2],
            F::LideDrive3 => self.lide_drive_is_dir[3],
            _ => false,
        }
    }

    /// Refresh a drive field's cached directory flag from its current path
    /// (the one host-filesystem stat the field's `_is_dir` companion is
    /// allowed: on the path actually changing, not on every draw). A no-op
    /// for a field this doesn't apply to.
    fn refresh_drive_is_dir(&mut self, field: LauncherField) {
        let is_dir = self.path(field).is_some_and(|p| p.is_dir());
        let slot = match field {
            F::IdeMaster => &mut self.ide_master_is_dir,
            F::IdeSlave => &mut self.ide_slave_is_dir,
            F::ScsiUnit0 => &mut self.scsi_unit_is_dir[0],
            F::ScsiUnit1 => &mut self.scsi_unit_is_dir[1],
            F::ScsiUnit2 => &mut self.scsi_unit_is_dir[2],
            F::ScsiUnit3 => &mut self.scsi_unit_is_dir[3],
            F::ScsiUnit4 => &mut self.scsi_unit_is_dir[4],
            F::ScsiUnit5 => &mut self.scsi_unit_is_dir[5],
            F::ScsiUnit6 => &mut self.scsi_unit_is_dir[6],
            F::CopperhfUnit0 => &mut self.copperhf_unit_is_dir[0],
            F::CopperhfUnit1 => &mut self.copperhf_unit_is_dir[1],
            F::CopperhfUnit2 => &mut self.copperhf_unit_is_dir[2],
            F::CopperhfUnit3 => &mut self.copperhf_unit_is_dir[3],
            F::CopperhfUnit4 => &mut self.copperhf_unit_is_dir[4],
            F::CopperhfUnit5 => &mut self.copperhf_unit_is_dir[5],
            F::CopperhfUnit6 => &mut self.copperhf_unit_is_dir[6],
            F::LideDrive0 => &mut self.lide_drive_is_dir[0],
            F::LideDrive1 => &mut self.lide_drive_is_dir[1],
            F::LideDrive2 => &mut self.lide_drive_is_dir[2],
            F::LideDrive3 => &mut self.lide_drive_is_dir[3],
            _ => return,
        };
        *slot = is_dir;
    }

    /// Set a drive field's directory-mount filesystem.
    pub fn set_drive_filesystem(&mut self, field: LauncherField, fs: crate::diskimage::FileSystem) {
        let slot = match field {
            F::IdeMaster => &mut self.ide_master_fs,
            F::IdeSlave => &mut self.ide_slave_fs,
            F::ScsiUnit0 => &mut self.scsi_unit_fs[0],
            F::ScsiUnit1 => &mut self.scsi_unit_fs[1],
            F::ScsiUnit2 => &mut self.scsi_unit_fs[2],
            F::ScsiUnit3 => &mut self.scsi_unit_fs[3],
            F::ScsiUnit4 => &mut self.scsi_unit_fs[4],
            F::ScsiUnit5 => &mut self.scsi_unit_fs[5],
            F::ScsiUnit6 => &mut self.scsi_unit_fs[6],
            F::CopperhfUnit0 => &mut self.copperhf_unit_fs[0],
            F::CopperhfUnit1 => &mut self.copperhf_unit_fs[1],
            F::CopperhfUnit2 => &mut self.copperhf_unit_fs[2],
            F::CopperhfUnit3 => &mut self.copperhf_unit_fs[3],
            F::CopperhfUnit4 => &mut self.copperhf_unit_fs[4],
            F::CopperhfUnit5 => &mut self.copperhf_unit_fs[5],
            F::CopperhfUnit6 => &mut self.copperhf_unit_fs[6],
            F::LideDrive0 => &mut self.lide_drive_fs[0],
            F::LideDrive1 => &mut self.lide_drive_fs[1],
            F::LideDrive2 => &mut self.lide_drive_fs[2],
            F::LideDrive3 => &mut self.lide_drive_fs[3],
            _ => return,
        };
        *slot = fs;
    }

    /// Flip a drive field's directory-mount filesystem between FFS and OFS
    /// -- the Storage tab's filesystem button is a two-way toggle, not a
    /// stepper, so it needs no forward/backward direction.
    pub fn cycle_drive_filesystem(&mut self, field: LauncherField) {
        let next = if self.drive_filesystem(field).ffs {
            crate::diskimage::FileSystem::OFS
        } else {
            crate::diskimage::FileSystem::FFS
        };
        self.set_drive_filesystem(field, next);
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
            F::CopperhfUnit0 => &mut self.copperhf_unit_names[0],
            F::CopperhfUnit1 => &mut self.copperhf_unit_names[1],
            F::CopperhfUnit2 => &mut self.copperhf_unit_names[2],
            F::CopperhfUnit3 => &mut self.copperhf_unit_names[3],
            F::CopperhfUnit4 => &mut self.copperhf_unit_names[4],
            F::CopperhfUnit5 => &mut self.copperhf_unit_names[5],
            F::CopperhfUnit6 => &mut self.copperhf_unit_names[6],
            F::LideDrive0 => &mut self.lide_drive_names[0],
            F::LideDrive1 => &mut self.lide_drive_names[1],
            F::LideDrive2 => &mut self.lide_drive_names[2],
            F::LideDrive3 => &mut self.lide_drive_names[3],
            F::Filesys0Dir => &mut self.filesys_names[0],
            F::Filesys1Dir => &mut self.filesys_names[1],
            F::Filesys2Dir => &mut self.filesys_names[2],
            F::Filesys3Dir => &mut self.filesys_names[3],
            _ => return,
        };
        *slot = value;
    }

    /// The Input tab's live summary: which host input ends up driving
    /// each port under the chosen devices and joystick-input mode.
    /// Computed by the same routing function the runtime input pump
    /// uses, so the promise cannot drift from the behavior.
    pub fn input_routing_summary(&self) -> [String; 2] {
        let routing =
            crate::video::window::host_routing_for(self.port_devices, self.joystick_input_mode);
        std::array::from_fn(|port| {
            let source = if routing.gamepad_mouse == Some(port) {
                "the host mouse and the gamepad".to_string()
            } else if routing.mouse == Some(port) {
                "the host mouse".to_string()
            } else if routing.gamepad == Some(port) && routing.keyboard2 == Some(port) {
                "the gamepad (numpad keys without a pad)".to_string()
            } else if routing.gamepad == Some(port) {
                "the gamepad".to_string()
            } else if routing.keyboard == Some(port) {
                if self.port_devices[port].is_mouse() {
                    "cursor keys as a mouse (fire keys = buttons)".to_string()
                } else {
                    "cursor keys (Ctrl/RAlt = fire, LAlt = button 2)".to_string()
                }
            } else {
                match self.port_devices[port] {
                    PortDevice::Mouse | PortDevice::GamepadMouse => {
                        "nothing (flip Joystick input to keyboard)".to_string()
                    }
                    PortDevice::Joystick | PortDevice::Cd32Pad => {
                        "nothing (keyboard passes through to the Amiga)".to_string()
                    }
                    PortDevice::Analogue => {
                        "--pot-after scripting or the control protocol".to_string()
                    }
                    PortDevice::None => "nothing (empty port)".to_string(),
                }
            };
            format!("Port {} is driven by {}", port + 1, source)
        })
    }

    /// The value text shown on a row (the current enum/size/number; the file
    /// name or a placeholder for paths; On/Off for toggles).
    /// Whether the drive row's volume-name box applies: a name labels a
    /// directory mount's FFS volume, so a CD image (which attaches a
    /// CD-ROM drive) has nothing to name, and the WHDLoad paths mount
    /// under fixed volume names (WHDBoot:/WHDGame:).
    pub fn drive_name_applies(&self, field: LauncherField) -> bool {
        !field.is_whdload_path_field()
            && !self
                .path(field)
                .is_some_and(crate::config::is_cd_image_path)
    }

    /// Where a WHDLoad game's machine comes from: the slave's own header,
    /// or this configuration.
    pub fn whdload_machine(&self) -> crate::config::WhdloadMachine {
        self.whdload_machine
    }

    /// Set a path field's value (a floppy image replaces that drive's
    /// playlist with a single disk and wires the drive in).
    pub fn set_path(&mut self, field: LauncherField, path: PathBuf) {
        // Adding a hard-disk image to an empty slot seeds its boot priority from
        // the positional cascade, so drives added in the launcher (with no
        // config priorities of their own) do not all tie at 0. A slot that
        // already held an image keeps whatever priority it had.
        // A Paths row is a host preference, not part of the machine, and
        // shares nothing with the rest of this: it stores its directory and
        // saves, and none of the drive bookkeeping below applies.
        if field.is_paths_field() {
            self.set_paths_dir(field, path);
            self.apply_paths();
            return;
        }
        let seed_cascade = Self::is_drive_field(field)
            && self.path(field).is_none()
            && !crate::config::is_cd_image_path(&path);
        match field {
            F::Rom => self.rom = Some(path),
            F::Mt32ControlRom => self.mt32_control_rom = Some(path),
            #[cfg(feature = "coppersynth")]
            F::CsynthSoundfont => self.csynth_soundfont = Some(path),
            F::Mt32PcmRom => self.mt32_pcm_rom = Some(path),
            F::ExtendedRom => self.extended_rom = Some(path),
            F::FmvRom => {
                self.fmv_rom = Some(path);
                self.fmv_rom_disabled = false;
            }
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
            F::CopperhfUnit0 => self.copperhf_units[0] = Some(path),
            F::CopperhfUnit1 => self.copperhf_units[1] = Some(path),
            F::CopperhfUnit2 => self.copperhf_units[2] = Some(path),
            F::CopperhfUnit3 => self.copperhf_units[3] = Some(path),
            F::CopperhfUnit4 => self.copperhf_units[4] = Some(path),
            F::CopperhfUnit5 => self.copperhf_units[5] = Some(path),
            F::CopperhfUnit6 => self.copperhf_units[6] = Some(path),
            F::LideRom => {
                self.lide_rom = Some(path);
                self.lide_rom_disabled = false;
            }
            F::LideRomBank2 => {
                self.lide_rom_bank2 = Some(path);
                self.lide_rom_bank2_disabled = false;
            }
            F::LideDrive0 => self.lide_drives[0] = Some(path),
            F::LideDrive1 => self.lide_drives[1] = Some(path),
            F::LideDrive2 => self.lide_drives[2] = Some(path),
            F::LideDrive3 => self.lide_drives[3] = Some(path),
            F::CdImage => self.cd_image = Some(path),
            F::Cd32Nvram => self.cd32_nvram = Some(path),
            F::ParallelOutput => self.parallel_output = Some(path),
            F::WhdloadGame => self.whdload_game = Some(path),
            F::WhdloadKickstarts => self.whdload_kickstarts = Some(path),
            F::WhdloadLibrary => self.whdload_library = Some(path),
            F::WhdloadWhdPackage => self.whdload_whd_package = Some(path),
            F::WhdloadSkickPackage => self.whdload_skick_package = Some(path),
            F::WhdloadGames => self.whdload_games = Some(path),
            _ => {
                if let Some((slot, false)) = filesys_slot(field) {
                    self.filesys_dirs[slot] = Some(path);
                }
            }
        }
        self.refresh_drive_is_dir(field);
        if seed_cascade {
            if let Some(boot) = drive_boot_field(field) {
                if self.drive_bootpri(boot).is_none() && !self.drive_boot_off(boot) {
                    self.set_drive_bootpri(boot, hdd_boot_cascade(self.hdd_boot_rank(boot)));
                }
            }
        }
    }

    fn set_floppy(&mut self, idx: usize, path: PathBuf) {
        self.df_playlists[idx] = vec![path];
        // Wire the drive in if it was beyond the configured count.
        self.floppy_drives = self.floppy_drives.max(idx as u8 + 1);
    }

    /// Clear a path field's value.
    pub fn clear_path(&mut self, field: LauncherField) {
        // Cleared, a Paths row inherits again rather than pointing
        // nowhere: the entry goes out of `[paths]` entirely, so it
        // follows the default from then on instead of freezing today's.
        if let Some(entry) = Self::paths_entry(&mut self.paths, field) {
            *entry = None;
            self.apply_paths();
            return;
        }
        match field {
            F::Rom => self.rom = None,
            F::ExtendedRom => self.extended_rom = None,
            F::FmvRom => {
                self.fmv_rom = None;
                self.fmv_rom_disabled = false;
            }
            F::Mt32ControlRom => self.mt32_control_rom = None,
            #[cfg(feature = "coppersynth")]
            F::CsynthSoundfont => self.csynth_soundfont = None,
            F::Mt32PcmRom => self.mt32_pcm_rom = None,
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
            F::CopperhfUnit0 => self.copperhf_units[0] = None,
            F::CopperhfUnit1 => self.copperhf_units[1] = None,
            F::CopperhfUnit2 => self.copperhf_units[2] = None,
            F::CopperhfUnit3 => self.copperhf_units[3] = None,
            F::CopperhfUnit4 => self.copperhf_units[4] = None,
            F::CopperhfUnit5 => self.copperhf_units[5] = None,
            F::CopperhfUnit6 => self.copperhf_units[6] = None,
            F::LideRom => {
                self.lide_rom = None;
                self.lide_rom_disabled = false;
            }
            F::LideRomBank2 => {
                self.lide_rom_bank2 = None;
                self.lide_rom_bank2_disabled = false;
            }
            F::LideDrive0 => self.lide_drives[0] = None,
            F::LideDrive1 => self.lide_drives[1] = None,
            F::LideDrive2 => self.lide_drives[2] = None,
            F::LideDrive3 => self.lide_drives[3] = None,
            F::CdImage => self.cd_image = None,
            F::Cd32Nvram => self.cd32_nvram = None,
            F::ParallelOutput => self.parallel_output = None,
            F::WhdloadGame => self.whdload_game = None,
            F::WhdloadKickstarts => self.whdload_kickstarts = None,
            F::WhdloadWhdPackage => self.whdload_whd_package = None,
            F::WhdloadSkickPackage => self.whdload_skick_package = None,
            F::WhdloadGames => self.whdload_games = None,
            F::WhdloadLibrary => self.whdload_library = None,
            _ => {
                if let Some((slot, false)) = filesys_slot(field) {
                    self.filesys_dirs[slot] = None;
                    // Boot priority or read-only on a mount with no directory is
                    // meaningless; reset both so a cleared slot emits nothing.
                    self.filesys_bootpri[slot] = -128;
                    self.filesys_readonly[slot] = false;
                }
            }
        }
        // A drive's volume name, filesystem, and boot priority are
        // meaningless once its image is gone.
        if Self::is_drive_field(field) {
            self.set_drive_name(field, String::new());
            self.set_drive_filesystem(field, crate::diskimage::FileSystem::FFS);
            self.clear_drive_bootpri(field);
            self.refresh_drive_is_dir(field);
        }
    }

    /// Reset a hard-disk drive's boot priority to unset (shown as 0) and its
    /// Bootable flag back on. Called when clearing the image (the priority has
    /// nothing to attach to). `field` is either the drive field or its
    /// boot-priority twin.
    fn clear_drive_bootpri(&mut self, field: LauncherField) {
        use LauncherField as F;
        match field {
            F::IdeMaster | F::IdeMasterBoot => {
                self.ide_master_bootpri = None;
                self.ide_master_boot_off = false;
            }
            F::IdeSlave | F::IdeSlaveBoot => {
                self.ide_slave_bootpri = None;
                self.ide_slave_boot_off = false;
            }
            _ => {
                if let Some(i) = scsi_boot_index(field) {
                    self.scsi_unit_bootpri[i] = None;
                    self.scsi_unit_boot_off[i] = false;
                } else if let Some(i) = lide_drive_index(field) {
                    self.lide_drive_bootpri[i] = None;
                    self.lide_drive_boot_off[i] = false;
                } else if let Some(i) = copperhf_unit_index(field) {
                    self.copperhf_unit_bootpri[i] = None;
                    self.copperhf_unit_boot_off[i] = false;
                }
            }
        }
    }

    /// What a hard-disk drive slot holds, if anything: an image path, or a
    /// real host disk. Boot priority applies to either -- a real disk takes
    /// its place in the boot order the same way an image does.
    fn drive_holds(&self, drive: LauncherField) -> Option<DriveContents<'_>> {
        if self.host_disk_on_row(drive).is_some() {
            return Some(DriveContents::HostDisk);
        }
        self.path(drive).map(DriveContents::Image)
    }

    /// The hard-disk drive field a boot-priority field belongs to (its twin on
    /// the Storage tab), or None when `field` is not a boot-priority field.
    fn boot_field_drive(field: LauncherField) -> Option<LauncherField> {
        use LauncherField as F;
        Some(match field {
            F::IdeMasterBoot => F::IdeMaster,
            F::IdeSlaveBoot => F::IdeSlave,
            F::ScsiUnit0Boot => F::ScsiUnit0,
            F::ScsiUnit1Boot => F::ScsiUnit1,
            F::ScsiUnit2Boot => F::ScsiUnit2,
            F::ScsiUnit3Boot => F::ScsiUnit3,
            F::ScsiUnit4Boot => F::ScsiUnit4,
            F::ScsiUnit5Boot => F::ScsiUnit5,
            F::ScsiUnit6Boot => F::ScsiUnit6,
            F::LideDrive0Boot => F::LideDrive0,
            F::LideDrive1Boot => F::LideDrive1,
            F::LideDrive2Boot => F::LideDrive2,
            F::LideDrive3Boot => F::LideDrive3,
            F::CopperhfUnit0Boot => F::CopperhfUnit0,
            F::CopperhfUnit1Boot => F::CopperhfUnit1,
            F::CopperhfUnit2Boot => F::CopperhfUnit2,
            F::CopperhfUnit3Boot => F::CopperhfUnit3,
            F::CopperhfUnit4Boot => F::CopperhfUnit4,
            F::CopperhfUnit5Boot => F::CopperhfUnit5,
            F::CopperhfUnit6Boot => F::CopperhfUnit6,
            _ => return None,
        })
    }

    /// The stored priority for a boot-priority field, apart from the Bootable
    /// flag: `None` is unset (shown as 0). Never the -128 sentinel -- that is
    /// [`Self::drive_boot_off`].
    fn drive_bootpri(&self, field: LauncherField) -> Option<i8> {
        use LauncherField as F;
        match field {
            F::IdeMasterBoot => self.ide_master_bootpri,
            F::IdeSlaveBoot => self.ide_slave_bootpri,
            F::ScsiUnit0Boot => self.scsi_unit_bootpri[0],
            F::ScsiUnit1Boot => self.scsi_unit_bootpri[1],
            F::ScsiUnit2Boot => self.scsi_unit_bootpri[2],
            F::ScsiUnit3Boot => self.scsi_unit_bootpri[3],
            F::ScsiUnit4Boot => self.scsi_unit_bootpri[4],
            F::ScsiUnit5Boot => self.scsi_unit_bootpri[5],
            F::ScsiUnit6Boot => self.scsi_unit_bootpri[6],
            _ => {
                if let Some(i) = lide_drive_index(field) {
                    self.lide_drive_bootpri[i]
                } else {
                    copperhf_unit_index(field).and_then(|i| self.copperhf_unit_bootpri[i])
                }
            }
        }
    }

    /// Store a drive's priority; `None` is unset (shown as 0, written as no key).
    pub fn set_drive_bootpri(&mut self, field: LauncherField, value: Option<i8>) {
        use LauncherField as F;
        match field {
            F::IdeMasterBoot => self.ide_master_bootpri = value,
            F::IdeSlaveBoot => self.ide_slave_bootpri = value,
            F::ScsiUnit0Boot => self.scsi_unit_bootpri[0] = value,
            F::ScsiUnit1Boot => self.scsi_unit_bootpri[1] = value,
            F::ScsiUnit2Boot => self.scsi_unit_bootpri[2] = value,
            F::ScsiUnit3Boot => self.scsi_unit_bootpri[3] = value,
            F::ScsiUnit4Boot => self.scsi_unit_bootpri[4] = value,
            F::ScsiUnit5Boot => self.scsi_unit_bootpri[5] = value,
            F::ScsiUnit6Boot => self.scsi_unit_bootpri[6] = value,
            _ => {
                if let Some(i) = lide_drive_index(field) {
                    self.lide_drive_bootpri[i] = value;
                } else if let Some(i) = copperhf_unit_index(field) {
                    self.copperhf_unit_bootpri[i] = value;
                }
            }
        }
    }

    /// Whether a boot-priority field is editable: its drive holds a hard-disk
    /// image. Priority is meaningless for an empty slot or a CD image.
    fn boot_field_applies(&self, field: LauncherField) -> bool {
        match Self::boot_field_drive(field).and_then(|drive| self.drive_holds(drive)) {
            Some(DriveContents::Image(p)) => !crate::config::is_cd_image_path(p),
            Some(DriveContents::HostDisk) => false,
            None => false,
        }
    }

    /// Whether any Boot Priority row is editable -- a hard-disk drive is present.
    /// The page's info text is hidden when it is not, since there is nothing to
    /// manage.
    pub fn has_boot_priority_rows(&self) -> bool {
        BOOTPRI_ROWS
            .iter()
            .any(|r| self.boot_field_applies(r.field))
    }

    /// Drives listed on the Boot Priority page: the two IDE bays, which stand
    /// their ground empty, plus every SCSI unit and Lide slot carrying a disk.
    pub fn boot_priority_row_count(&self) -> usize {
        BOOTPRI_ROWS
            .iter()
            .filter(|r| !self.row_hidden(r.field))
            .count()
    }

    /// Whether the boot order runs onto a second page -- which is also
    /// whether the first one shows its "Next Page >" button.
    pub fn boot_priority_has_second_page(&self) -> bool {
        self.boot_priority_row_count() > BOOTPRI_PAGE_ROWS
    }

    /// How many Boot Priority pages the listed drives need, at least one.
    /// However many board families are fitted, the table pages rather than
    /// piling up on (or falling off the end of) a fixed two.
    pub fn boot_priority_page_count(&self) -> usize {
        self.boot_priority_row_count()
            .div_ceil(BOOTPRI_PAGE_ROWS)
            .max(1)
    }

    /// The 1-based page number a Boot Priority tab stands for, or `None` for
    /// any other tab.
    fn boot_priority_page_number(tab: LauncherTab) -> Option<usize> {
        Some(match tab {
            LauncherTab::BootPriority => 1,
            LauncherTab::BootPriorityMore(n) => usize::from(n),
            _ => return None,
        })
    }

    /// The tab for a 1-based Boot Priority page number.
    fn boot_priority_tab(page: usize) -> LauncherTab {
        if page <= 1 {
            LauncherTab::BootPriority
        } else {
            LauncherTab::BootPriorityMore(page as u8)
        }
    }

    /// The tab a Boot Priority page's "Next Page >" button goes to, or
    /// `None` when `tab` is not a Boot Priority page or is already the last
    /// one. Every page's own Back button already returns to the first, so
    /// paging only ever needs to go forward.
    pub fn boot_priority_next_page(&self, tab: LauncherTab) -> Option<LauncherTab> {
        let page = Self::boot_priority_page_number(tab)?;
        (page < self.boot_priority_page_count()).then(|| Self::boot_priority_tab(page + 1))
    }

    /// The page a session should stand on after this setup replaced the one
    /// it was looking at (Load..., Defaults): the same page, unless that
    /// page no longer exists -- a boot page with nothing left on it falls
    /// back to the first rather than standing empty.
    pub fn settle_tab(&self, tab: LauncherTab) -> LauncherTab {
        match Self::boot_priority_page_number(tab) {
            Some(page) if page > self.boot_priority_page_count() => LauncherTab::BootPriority,
            _ => tab,
        }
    }

    /// Which Boot Priority page a drive's row falls on, or `None` for a row
    /// that is not one of them (the column titles, and every other page's
    /// rows). Paged by position among the drives actually listed, not by
    /// position in the table, so a machine with a Lide board but no SCSI
    /// keeps its four Lide drives on the first page.
    ///
    /// Only meaningful for a row that is listed: asked about a hidden one
    /// it answers with the page of the rank the row *would* take. Callers
    /// go through [`Self::row_on_page`], which checks `row_hidden` first.
    pub fn boot_page_of(&self, field: LauncherField) -> Option<LauncherTab> {
        if !BOOTPRI_ROWS.iter().any(|r| r.field == field) {
            return None;
        }
        let rank = BOOTPRI_ROWS
            .iter()
            .take_while(|r| r.field != field)
            .filter(|r| !self.row_hidden(r.field))
            .count();
        Some(Self::boot_priority_tab(rank / BOOTPRI_PAGE_ROWS + 1))
    }

    /// Whether a row is drawn on `tab`: present on this machine, and -- for
    /// the paged boot order -- belonging to this page rather than another.
    pub fn row_on_page(&self, tab: LauncherTab, field: LauncherField) -> bool {
        !self.row_hidden(field) && self.boot_page_of(field).is_none_or(|page| page == tab)
    }

    /// Whether a drive's Bootable box is cleared -- the config's -128 sentinel,
    /// which mounts the volume but keeps it out of the boot vote.
    pub fn drive_boot_off(&self, field: LauncherField) -> bool {
        use LauncherField as F;
        match field {
            F::IdeMasterBoot => self.ide_master_boot_off,
            F::IdeSlaveBoot => self.ide_slave_boot_off,
            _ => {
                scsi_boot_index(field).is_some_and(|i| self.scsi_unit_boot_off[i])
                    || lide_drive_index(field).is_some_and(|i| self.lide_drive_boot_off[i])
                    || copperhf_unit_index(field).is_some_and(|i| self.copperhf_unit_boot_off[i])
            }
        }
    }

    /// The value the config stores for a drive: the -128 sentinel when its
    /// Bootable box is cleared, otherwise its priority (unset omits the key).
    fn effective_bootpri(&self, field: LauncherField) -> Option<i8> {
        if self.drive_boot_off(field) {
            Some(BOOT_PRI_NEVER)
        } else {
            self.drive_bootpri(field)
        }
    }

    /// Flip a drive's Bootable box. Clearing it shows the -128 sentinel the
    /// config will store, while keeping the priority underneath so re-ticking
    /// restores it within the session.
    pub fn toggle_drive_boot(&mut self, field: LauncherField) {
        let off = self.drive_boot_off(field);
        self.set_drive_boot_off(field, !off);
    }

    /// Set a drive's Bootable box (cleared = the -128 sentinel).
    fn set_drive_boot_off(&mut self, field: LauncherField, off: bool) {
        use LauncherField as F;
        match field {
            F::IdeMasterBoot => self.ide_master_boot_off = off,
            F::IdeSlaveBoot => self.ide_slave_boot_off = off,
            _ => {
                if let Some(i) = scsi_boot_index(field) {
                    self.scsi_unit_boot_off[i] = off;
                } else if let Some(i) = lide_drive_index(field) {
                    self.lide_drive_boot_off[i] = off;
                } else if let Some(i) = copperhf_unit_index(field) {
                    self.copperhf_unit_boot_off[i] = off;
                }
            }
        }
    }

    /// Number of present hard-disk drives (images, not CD-ROMs) ahead of `field`
    /// in the Boot Priority list order -- its rank for the cascade default.
    fn hdd_boot_rank(&self, field: LauncherField) -> usize {
        BOOTPRI_ROWS
            .iter()
            .take_while(|r| r.field != field)
            .filter(|r| self.boot_field_applies(r.field))
            .count()
    }

    /// The bay a `DfNImage` field belongs to.
    pub fn drive_image_bay(field: LauncherField) -> Option<usize> {
        Some(match field {
            F::Df0Image => 0,
            F::Df1Image => 1,
            F::Df2Image => 2,
            F::Df3Image => 3,
            _ => return None,
        })
    }

    /// The bay a `DfNWriteProtect` field belongs to.
    pub fn drive_protect_bay(field: LauncherField) -> Option<usize> {
        Some(match field {
            F::Df0WriteProtect => 0,
            F::Df1WriteProtect => 1,
            F::Df2WriteProtect => 2,
            F::Df3WriteProtect => 3,
            _ => return None,
        })
    }

    /// The `DfNBridge` tick box for a bay.
    pub fn drive_bridge_field(idx: usize) -> LauncherField {
        match idx {
            0 => F::Df0Bridge,
            1 => F::Df1Bridge,
            2 => F::Df2Bridge,
            _ => F::Df3Bridge,
        }
    }

    /// Whether this bay uses a real drive rather than an image.
    pub fn drive_bridged(&self, idx: usize) -> bool {
        self.df_bridge.get(idx).is_some_and(Option::is_some)
    }

    /// Turn a bay over to a physical drive, or back to images. Switching to a
    /// bridge clears the image: the disk in the drive is the media now.
    ///
    /// Without the feature there is no such thing as a bridged bay -- no tick
    /// box offers one, the config file's keys are ignored -- and this refuses
    /// as well, so no path can leave a bay in a state the build cannot honour.
    #[cfg(feature = "fluxbridge")]
    pub fn set_drive_bridged(&mut self, idx: usize, on: bool) {
        if idx >= self.df_bridge.len() || self.drive_bridged(idx) == on {
            return;
        }
        if on {
            self.df_playlists[idx].clear();
            self.df_bridge[idx] = Some(FluxBridgeConfig::default());
            // Look again now: this is the moment the user expects to be told
            // whether there is anything on the other end.
            self.bridge_status = bridge_status();
            #[cfg(feature = "fluxbridge")]
            {
                self.bridge_ports = sample_bridge_ports();
            }
            // With nothing on the other end, the honest interface is "None";
            // the row is there to change once something is plugged in.
            self.df_bridge_none[idx] = self.bridge_status == BridgeStatus::NoInterface;
            // Wire the bay in, as choosing an image would.
            self.floppy_drives = self.floppy_drives.max(idx as u8 + 1);
        } else {
            self.df_bridge[idx] = None;
            self.df_bridge_none[idx] = false;
        }
    }

    #[cfg(not(feature = "fluxbridge"))]
    pub fn set_drive_bridged(&mut self, _idx: usize, _on: bool) {}

    /// The interface a bridged bay is set to use, for its media row. Naming one
    /// that is not there would suggest the bay is ready to run when it is not,
    /// so what is missing is named instead -- and which of the two it is
    /// decides whether the fix is installing software or plugging in hardware.
    pub fn drive_bridge_label(&self, idx: usize) -> String {
        let Some(cfg) = self.df_bridge.get(idx).and_then(Option::as_ref) else {
            return "(none)".to_string();
        };
        if self.df_bridge_none[idx] {
            return "Not connected".to_string();
        }
        match self.bridge_status {
            BridgeStatus::NoInterface => "Not connected".to_string(),
            BridgeStatus::Attached => cfg.driver.label().to_string(),
        }
    }

    /// What the library can see, for the FluxBridge page's heading.
    pub fn bridge_status(&self) -> BridgeStatus {
        self.bridge_status
    }

    /// Which bay the settings page is editing.
    pub fn bridge_edit_drive(&self) -> usize {
        self.bridge_edit_drive
    }

    /// The controller the SCSI row is showing.
    #[cfg(test)]
    fn scsi_controller_for_test(&self) -> Option<ScsiController> {
        self.scsi_controller
    }

    /// Stand in for the host's storage, so the page can be drawn and tested
    /// without one attached.
    #[cfg(test)]
    pub fn set_host_disks_for_test(&mut self, disks: Vec<HostDiskRow>) {
        self.host_disks = disks;
    }

    /// The disks the Host Disk table is showing.
    /// Fill the host-disk list with made-up rows, for tests that need
    /// the page's list without the host having any disks to scan.
    #[cfg(test)]
    pub(crate) fn fake_host_disks(&mut self, rows: usize) {
        self.host_disks = (0..rows)
            .map(|i| HostDiskRow {
                id: format!("disk{i}"),
                fingerprint: None,
                volume: format!("Volume {i}"),
                size: "1.0 GB".to_string(),
                mounted: Vec::new(),
                writable: false,
                attach: None,
            })
            .collect();
    }

    pub fn host_disks(&self) -> &[HostDiskRow] {
        &self.host_disks
    }

    /// First row shown in the table.
    pub fn host_disk_scroll(&self) -> usize {
        self.host_disk_scroll
    }

    pub fn host_disk_scroll_rate(&mut self) -> &mut ScrollRate {
        &mut self.host_disk_scroll_rate
    }

    /// Move the window over the list, stopping at either end. `visible` is
    /// how many rows the box shows, which is the caller's geometry to know.
    pub fn scroll_host_disks(&mut self, delta: isize, visible: usize) {
        let last_start = self.host_disks.len().saturating_sub(visible);
        let at = self.host_disk_scroll as isize + delta;
        self.host_disk_scroll = at.clamp(0, last_start as isize) as usize;
    }

    /// Look at the host's storage again. Called when the page opens and from
    /// its Refresh button, so a card pushed in mid-session appears without the
    /// launcher polling for it.
    pub fn refresh_host_disks(&mut self) {
        // Looking again should not undo what was chosen: a disk still present
        // keeps the read-only and attachment it was given.
        let previous = std::mem::take(&mut self.host_disks);
        self.host_disks = sample_host_disks();
        self.reconcile_host_disk_rows(&previous);
    }

    fn reconcile_host_disk_rows(&mut self, previous: &[HostDiskRow]) {
        let selected_before: Vec<(&str, Option<&str>)> = previous
            .iter()
            .filter(|row| self.host_disk_selected.contains(&row.id))
            .map(|row| (row.id.as_str(), row.fingerprint.as_deref()))
            .collect();
        // A shorter list must not leave the window past its end.
        self.host_disk_scroll = self.host_disk_scroll.min(self.host_disks.len());
        for row in &mut self.host_disks {
            if let Some(old) = previous.iter().find(|old| {
                match (row.fingerprint.as_deref(), old.fingerprint.as_deref()) {
                    (Some(new), Some(old)) => new == old,
                    _ => old.id == row.id,
                }
            }) {
                row.writable = old.writable;
                row.attach = old.attach;
            }
        }
        // A disk that has gone (unplugged between looks) cannot stay ticked.
        self.host_disk_selected = self
            .host_disks
            .iter()
            .filter(|row| {
                selected_before.iter().any(|(old_id, old_fingerprint)| {
                    match (row.fingerprint.as_deref(), *old_fingerprint) {
                        (Some(new), Some(old)) => new == old,
                        _ => row.id == *old_id,
                    }
                })
            })
            .map(|row| row.id.clone())
            .collect();
        // The page shows the machine as it stands: a disk already given to it
        // is ticked, sitting where it was put.
        for attached in &mut self.host_disks_attached {
            if let Some(row) = self.host_disks.iter_mut().find(|row| {
                attached
                    .fingerprint
                    .as_ref()
                    .zip(row.fingerprint.as_ref())
                    .is_some_and(|(saved, current)| saved == current)
                    || ((attached.fingerprint.is_none() || row.fingerprint.is_none())
                        && row.id == attached.device)
            }) {
                attached.device = row.id.clone();
                attached.fingerprint = row.fingerprint.clone();
                row.attach = Some(attached.attach);
                row.writable = attached.writable;
                if !self.host_disk_selected.contains(&attached.device) {
                    self.host_disk_selected.push(attached.device.clone());
                }
            }
        }
    }

    /// The real disks currently given to the machine.
    pub fn host_disks_attached(&self) -> &[crate::config::HostDiskConfig] {
        &self.host_disks_attached
    }

    /// What is attached at one point, if anything.
    pub fn host_disk_at(
        &self,
        attach: crate::config::HostDiskAttach,
    ) -> Option<&crate::config::HostDiskConfig> {
        self.host_disks_attached.iter().find(|d| d.attach == attach)
    }

    /// Whether a disk is currently given to the machine.
    pub fn host_disk_is_attached(&self, device: &str) -> bool {
        self.host_disks_attached.iter().any(|d| d.device == device)
    }

    /// Whether a disk is ticked.
    pub fn host_disk_is_selected(&self, device: &str) -> bool {
        self.host_disk_selected.iter().any(|d| d == device)
    }

    /// The ticked disks, in the order they were ticked.
    pub fn host_disks_selected(&self) -> &[String] {
        &self.host_disk_selected
    }

    /// Whether the machine has the port an attachment point needs.
    fn attach_is_fitted(&self, attach: crate::config::HostDiskAttach) -> bool {
        use crate::config::HostDiskAttach as A;
        match attach {
            A::IdeMaster | A::IdeSlave => self.has_ide(),
            A::Scsi(_) => self.has_scsi_controller(),
            A::LideMaster(ch) | A::LideSlave(ch) => self
                .lide_board
                .is_some_and(|b| usize::from(ch) < b.channels()),
        }
    }

    /// The first attachment point nothing has claimed, preferring the ones
    /// the machine can actually use so a tick lands somewhere useful.
    fn free_host_disk_attach(&self) -> Option<crate::config::HostDiskAttach> {
        let claimed: Vec<_> = self
            .host_disks
            .iter()
            .filter(|row| self.host_disk_is_selected(&row.id))
            .filter_map(|row| row.attach)
            .collect();
        crate::config::HostDiskAttach::all()
            .into_iter()
            .filter(|a| !claimed.contains(a))
            .find(|a| self.attach_is_fitted(*a))
    }

    /// Give back any real disk whose attachment point the machine no longer
    /// has. Taking the SCSI controller out, or moving to a model with no IDE
    /// port, leaves a disk configured somewhere nothing can reach it; the
    /// disk quietly goes rather than being carried to Run and refused there.
    fn drop_unreachable_host_disks(&mut self) {
        let gone: Vec<_> = self
            .host_disks_attached
            .iter()
            .filter(|disk| !self.attach_is_fitted(disk.attach))
            .map(|disk| (disk.device.clone(), disk.attach))
            .collect();
        for (device, attach) in gone {
            log::info!(
                "host disk: {device} taken off {} -- the machine no longer has it",
                attach.label()
            );
            self.host_disks_attached.retain(|d| d.attach != attach);
            self.host_disk_selected.retain(|id| *id != device);
            if let Some(row) = self.host_disks.iter_mut().find(|d| d.id == device) {
                row.attach = None;
            }
            // This is an unmount like any other, so the hold goes with it.
            // Leaving it would keep the disk from the host with nothing left
            // on screen to release it -- no row, no tick, no Unmount.
            #[cfg(not(target_arch = "wasm32"))]
            crate::blockdev::release_device(&device);
        }
    }

    /// Why the last tick did not take, if it did not.
    pub fn host_disk_warning(&self) -> Option<&str> {
        self.host_disk_warning.as_deref()
    }

    /// Tick or untick one disk.
    ///
    /// Ticking is what gives the disk a place: the first attachment point
    /// still free, IDE Master before anything else. Unticking takes the
    /// place away again, and the cell reads blank -- an unticked disk is
    /// going nowhere, and a place named on one would be a claim nothing is
    /// making.
    pub fn select_host_disk(&mut self, index: usize) {
        let Some(id) = self.host_disks.get(index).map(|d| d.id.clone()) else {
            return;
        };
        self.host_disk_warning = None;
        if self.host_disk_is_selected(&id) {
            self.host_disk_selected.retain(|d| *d != id);
            if let Some(row) = self.host_disks.get_mut(index) {
                row.attach = None;
            }
            return;
        }
        // A disk the machine already has goes back to the place it holds:
        // ticking it again is recognising the attachment, not asking for a
        // second one somewhere new.
        if let Some(attached) = self
            .host_disks_attached
            .iter()
            .find(|d| d.device == id)
            .map(|d| d.attach)
        {
            if let Some(row) = self.host_disks.get_mut(index) {
                row.attach = Some(attached);
            }
            self.host_disk_selected.push(id);
            return;
        }
        match self.free_host_disk_attach() {
            Some(free) => {
                if let Some(row) = self.host_disks.get_mut(index) {
                    row.attach = Some(free);
                }
                self.host_disk_selected.push(id);
            }
            // Nowhere to put this disk, so the tick does not take and the
            // reason is shown: silently ticking a disk that could not be
            // attached would be a lie found out only at Mount. Which reason
            // depends on why -- a machine with no port at all is a different
            // problem from one whose ports are full.
            None => {
                let any_port = crate::config::HostDiskAttach::all()
                    .into_iter()
                    .any(|a| self.attach_is_fitted(a));
                self.host_disk_warning = Some(if any_port {
                    "Every attachment point is already in use".to_string()
                } else {
                    crate::config::HostDiskAttach::no_port_requirement().to_string()
                });
            }
        }
    }

    /// What to call a disk that is attached: the volume the host reported for
    /// it, or its identifier when it is not attached to this computer now --
    /// a configuration outlives the card reader it was written at.
    pub fn host_disk_volume(&self, device: &str) -> String {
        self.host_disks
            .iter()
            .find(|d| d.id == device)
            .map(|d| d.volume.clone())
            .unwrap_or_else(|| device.to_string())
    }

    /// How a slot holding a real disk reads: the device the host calls it,
    /// and the volume on it. A configuration can outlive the disk it names,
    /// so one that is not there says so rather than reading as normal.
    pub fn host_disk_label(&self, device: &str) -> String {
        match self.host_disks.iter().find(|d| d.id == device) {
            // The volume alone: the row's own label already says which drive
            // this is, and Unmount beside it already says it is a real disk.
            // (On Windows the "volume" is the model string the bridge
            // reports, `Generic MassStorageClass USB Device` and the like --
            // there is nothing shorter that still names the hardware.)
            Some(disk) => disk.volume.clone(),
            // Nothing has been sampled yet, so nothing is known: the Host Disk
            // page looks when it opens, and a launcher that has not been there
            // has not asked. A disk sitting in the reader called "not
            // connected" is worse than one described only by its name.
            None if self.host_disks.is_empty() => device.to_string(),
            None => format!("{device} (not connected)"),
        }
    }

    /// Give every ticked disk to the machine.
    ///
    /// A slot holds one thing, so whatever was there -- another disk, or an
    /// image -- makes way. Disks whose attachment point the machine does not
    /// have are left alone and reported: configuring a disk somewhere it can
    /// never be reached is worse than saying so.
    pub fn mount_host_disks(&mut self) -> Result<Vec<crate::config::HostDiskConfig>, String> {
        if self.host_disk_selected.is_empty() {
            return Err("Select a disk to attach it to the machine".to_string());
        }
        // A ticked disk always has a place -- ticking is what assigns one --
        // so a tick without one is a bug worth failing loudly on, not a case.
        let chosen: Vec<(HostDiskRow, crate::config::HostDiskAttach)> = self
            .host_disks
            .iter()
            .filter(|row| self.host_disk_is_selected(&row.id))
            .map(|row| {
                let attach = row.attach.expect("a ticked disk has an attachment point");
                (row.clone(), attach)
            })
            .collect();
        if let Some((_, bad)) = chosen
            .iter()
            .find(|(_, attach)| !self.attach_is_fitted(*attach))
        {
            return Err(bad.requirement().to_string());
        }

        let mut attached = Vec::new();
        for (row, attach) in chosen {
            let entry = crate::config::HostDiskConfig {
                device: row.id.clone(),
                fingerprint: row.fingerprint.clone(),
                identity_confirmed: true,
                attach,
                writable: row.writable,
            };
            self.host_disks_attached
                .retain(|d| d.attach != entry.attach);
            self.clear_slot(entry.attach);
            self.host_disks_attached.push(entry.clone());
            attached.push(entry);
        }
        Ok(attached)
    }

    /// Empty whatever image was in a slot a disk is taking.
    fn clear_slot(&mut self, attach: crate::config::HostDiskAttach) {
        match attach {
            crate::config::HostDiskAttach::IdeMaster => {
                self.ide_master = None;
                self.ide_master_name = None;
            }
            crate::config::HostDiskAttach::IdeSlave => {
                self.ide_slave = None;
                self.ide_slave_name = None;
            }
            crate::config::HostDiskAttach::LideMaster(ch)
            | crate::config::HostDiskAttach::LideSlave(ch) => {
                let idx = usize::from(ch) * 2
                    + usize::from(matches!(
                        attach,
                        crate::config::HostDiskAttach::LideSlave(_)
                    ));
                if let Some(slot) = self.lide_drives.get_mut(idx) {
                    *slot = None;
                }
                if let Some(name) = self.lide_drive_names.get_mut(idx) {
                    *name = None;
                }
            }
            crate::config::HostDiskAttach::Scsi(unit) => {
                if let Some(slot) = self.scsi_units.get_mut(usize::from(unit)) {
                    *slot = None;
                }
                if let Some(name) = self.scsi_unit_names.get_mut(usize::from(unit)) {
                    *name = None;
                }
            }
        }
    }

    /// Take a disk back off the machine and hand it to the host.
    pub fn unmount_host_disk(&mut self, attach: crate::config::HostDiskAttach) -> Option<String> {
        let at = self
            .host_disks_attached
            .iter()
            .position(|d| d.attach == attach)?;
        let device = self.host_disks_attached.remove(at).device;
        // The Host Disk page mirrors what is attached, so taking the disk
        // off the machine unticks it there too -- a tick surviving the
        // unmount would read as still attached.
        self.host_disk_selected.retain(|id| *id != device);
        if let Some(row) = self.host_disks.iter_mut().find(|d| d.id == device) {
            row.attach = None;
        }
        Some(device)
    }

    /// Take every ticked disk that the machine has back off it, and say
    /// which went. The Host Disk page's Unmount: the ticks say which disks
    /// are meant, exactly as they do for Mount.
    pub fn unmount_selected_host_disks(&mut self) -> Vec<String> {
        let attached: Vec<crate::config::HostDiskAttach> = self
            .host_disks_attached
            .iter()
            .filter(|d| self.host_disk_is_selected(&d.device))
            .map(|d| d.attach)
            .collect();
        attached
            .into_iter()
            .filter_map(|attach| self.unmount_host_disk(attach))
            .collect()
    }

    /// Flip whether the guest may write to one disk.
    pub fn toggle_host_disk_writable(&mut self, index: usize) {
        if let Some(row) = self.host_disks.get_mut(index) {
            row.writable = !row.writable;
        }
    }

    /// Step one disk's attachment point, skipping the ones another ticked
    /// disk has already claimed so the cycle only offers places it can go.
    pub fn cycle_host_disk_attach(&mut self, index: usize, forward: bool) {
        let Some(row) = self.host_disks.get(index) else {
            return;
        };
        // Only a ticked disk has a place to step: the cell is blank
        // otherwise, and clicking blank is not a request anybody made.
        let (Some(current), true) = (row.attach, self.host_disk_is_selected(&row.id)) else {
            return;
        };
        let id = row.id.clone();
        let claimed: Vec<_> = self
            .host_disks
            .iter()
            .filter(|r| r.id != id && self.host_disk_is_selected(&r.id))
            .filter_map(|r| r.attach)
            .collect();
        let options: Vec<_> = crate::config::HostDiskAttach::all()
            .into_iter()
            .filter(|a| *a == current || !claimed.contains(a))
            .collect();
        let at = options.iter().position(|a| *a == current).unwrap_or(0);
        let next = if forward {
            (at + 1) % options.len()
        } else {
            (at + options.len() - 1) % options.len()
        };
        if let Some(row) = self.host_disks.get_mut(index) {
            row.attach = Some(options[next]);
        }
    }

    /// The attachment point a Storage-page row stands for, for the rows that
    /// can hold a real disk.
    pub fn host_disk_attach_of(field: F) -> Option<crate::config::HostDiskAttach> {
        match field {
            F::IdeMaster => Some(crate::config::HostDiskAttach::IdeMaster),
            F::IdeSlave => Some(crate::config::HostDiskAttach::IdeSlave),
            F::LideDrive0 => Some(crate::config::HostDiskAttach::LideMaster(0)),
            F::LideDrive1 => Some(crate::config::HostDiskAttach::LideSlave(0)),
            F::LideDrive2 => Some(crate::config::HostDiskAttach::LideMaster(1)),
            F::LideDrive3 => Some(crate::config::HostDiskAttach::LideSlave(1)),
            F::ScsiUnit0 => Some(crate::config::HostDiskAttach::Scsi(0)),
            F::ScsiUnit1 => Some(crate::config::HostDiskAttach::Scsi(1)),
            F::ScsiUnit2 => Some(crate::config::HostDiskAttach::Scsi(2)),
            F::ScsiUnit3 => Some(crate::config::HostDiskAttach::Scsi(3)),
            F::ScsiUnit4 => Some(crate::config::HostDiskAttach::Scsi(4)),
            F::ScsiUnit5 => Some(crate::config::HostDiskAttach::Scsi(5)),
            F::ScsiUnit6 => Some(crate::config::HostDiskAttach::Scsi(6)),
            _ => None,
        }
    }

    /// The real disk sitting on a Storage-page row, if one is.
    pub fn host_disk_on_row(&self, field: F) -> Option<&crate::config::HostDiskConfig> {
        self.host_disk_at(Self::host_disk_attach_of(field)?)
    }

    pub fn set_bridge_edit_drive(&mut self, idx: usize) {
        self.bridge_edit_drive = idx.min(3);
    }

    /// Whether the bridge page is editing a bay with an interface actually
    /// selected and attached. Only then does that interface's own set of
    /// capabilities decide anything: with none there is nothing to have an
    /// opinion, and the rows it would shape have nothing to show.
    pub fn bridge_interface_selected(&self) -> bool {
        self.bridge_edit().is_some()
            && !self.df_bridge_none[self.bridge_edit_drive]
            && self.bridge_status == BridgeStatus::Attached
    }

    /// The settings being shown on the FluxBridge page.
    fn bridge_edit(&self) -> Option<&FluxBridgeConfig> {
        self.df_bridge[self.bridge_edit_drive].as_ref()
    }

    fn bridge_edit_mut(&mut self) -> Option<&mut FluxBridgeConfig> {
        self.df_bridge[self.bridge_edit_drive].as_mut()
    }

    /// Whether the interface this page is editing honours one of the library's
    /// optional settings, as that driver itself reports.
    ///
    /// Each driver advertises what it supports, and they differ: a Greaseweazle
    /// takes a drive-select cable, a DrawBridge answers to a different set.
    /// Offering a switch the hardware ignores is how a user ends up believing
    /// they changed something, so the ones it does not honour are greyed with
    /// the interface's name against them.
    #[cfg(feature = "fluxbridge")]
    fn bridge_driver_supports(&self, option: u32) -> bool {
        let Some(cfg) = self.bridge_edit() else {
            return false;
        };
        // A driver this build does not carry (a config written for another
        // build) cannot be asked, and leaving its rows live is better than
        // greying out a page the user cannot then fix.
        crate::fluxbridge::driver_named(cfg.driver.match_token())
            .is_none_or(|driver| driver.supports(option))
    }

    /// Serial ports to offer, "Automatic" first -- the default, and what
    /// every current interface supports. The library's own scan leads, then
    /// every other serial device the host has, so an interface on a chip the
    /// scan's naming misses -- an Arduino clone mounting as
    /// `tty.wchusbserial*` on macOS, say -- can still be picked by hand. The
    /// names are the host's own: `/dev/cu.usbmodem101` on macOS,
    /// `/dev/ttyACM0` on Linux, `COM3` on Windows.
    fn bridge_port_options(&self) -> Vec<Option<String>> {
        #[cfg(feature = "fluxbridge")]
        {
            self.bridge_ports.clone()
        }
        #[cfg(not(feature = "fluxbridge"))]
        {
            vec![None]
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

/// What a status line is saying, which is also how it is coloured.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusKind {
    /// It worked.
    Ok,
    /// It is still happening. Shown while a long piece of host work runs,
    /// so the panel says what it is waiting for rather than going quiet.
    Busy,
    /// It did not work, or will not.
    Error,
}

/// A short status/error line shown along the bottom of the configuration panel.
#[derive(Debug, Clone)]
pub struct StatusMessage {
    pub text: String,
    pub kind: StatusKind,
}

impl StatusMessage {
    pub fn ok(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            kind: StatusKind::Ok,
        }
    }

    pub fn busy(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            kind: StatusKind::Busy,
        }
    }

    pub fn err(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            kind: StatusKind::Error,
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
    /// A hard-disk boot priority typed as a number (the Boot Priority page).
    DriveBootpri(LauncherField),
    /// A word typed on a Create Image page: a volume name or a device name.
    NewImageText(LauncherField),
    /// The host half of a Serial section address (Connect or Listen).
    SerialHost(LauncherField),
    /// The port half of a Serial section address: up to five digits.
    SerialPort(LauncherField),
    /// The fixed 16-bit RAM power-on word on the Memory page.
    RamPattern,
    /// A session endpoint or shared game code.
    Netplay(LauncherField),
}

/// Where typing goes in a text field, as a character index into it.
///
/// One of these sits beside every editable line in the launcher -- the value
/// boxes on the configuration pages and the boxes in the WHDLoad dialogs --
/// so all of them insert, delete and step alike, and all of them draw the
/// same block over the character the caret is on. Without it a box can only
/// be typed at the end, which is no way to correct a long path or amend
/// metadata that is nearly right.
///
/// Characters, not bytes: the fields hold whatever a name or a title is
/// spelt with, and stepping half way into one would split it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Caret(usize);

impl Caret {
    /// Past the last character, which is where opening a box for editing
    /// puts it: what is already in there is usually a thing to add to.
    pub fn end_of(text: &str) -> Self {
        Self::at_end_of_len(text.chars().count())
    }

    /// The same for a line whose text is not to be handed out -- a masked
    /// password, which is counted rather than read.
    pub fn at_end_of_len(len: usize) -> Self {
        Self(len)
    }

    /// Which character the caret is on, for drawing the block.
    pub fn at(self) -> usize {
        self.0
    }

    /// Pull the caret back inside a line it may now be off the end of --
    /// the focus moved to a shorter field, or the value was replaced.
    fn clamp(&mut self, text: &str) {
        self.0 = self.0.min(text.chars().count());
    }

    pub fn left(&mut self) {
        self.0 = self.0.saturating_sub(1);
    }

    pub fn right(&mut self, text: &str) {
        self.right_of(text.chars().count());
    }

    /// Step right within a line of `len` characters.
    pub fn right_of(&mut self, len: usize) {
        self.0 = (self.0 + 1).min(len);
    }

    pub fn home(&mut self) {
        self.0 = 0;
    }

    pub fn end(&mut self, text: &str) {
        self.0 = text.chars().count();
    }

    /// The same for a counted line.
    pub fn end_of_len(&mut self, len: usize) {
        self.0 = len;
    }

    /// The byte offset the caret sits at, which is the end of the string
    /// when it is past the last character.
    fn byte_in(self, text: &str) -> usize {
        text.char_indices()
            .nth(self.0)
            .map_or(text.len(), |(at, _)| at)
    }

    /// Insert at the caret and step over what was typed.
    pub fn insert(&mut self, text: &mut String, c: char) {
        // Against a line that was changed without the caret being told --
        // a field reset, a value replaced wholesale -- rather than reaching
        // off the end of it.
        self.clamp(text);
        let at = self.byte_in(text);
        text.insert(at, c);
        self.0 += 1;
    }

    /// Delete the character before the caret, as Backspace does. False when
    /// there is nothing behind it.
    pub fn backspace(&mut self, text: &mut String) -> bool {
        self.clamp(text);
        if self.0 == 0 {
            return false;
        }
        self.left();
        let at = self.byte_in(text);
        text.remove(at);
        true
    }

    /// Delete the character the caret is on, as Delete does. False at the
    /// end of the line, where it is on nothing.
    pub fn delete(&mut self, text: &mut String) -> bool {
        let at = self.byte_in(text);
        if at >= text.len() {
            return false;
        }
        text.remove(at);
        true
    }
}

/// How fast a held scroll runs.
///
/// A list can be a few hundred rows and a keypress is one of them, so
/// reaching the far end a row at a time is a lot of pressing. Holding it
/// instead runs through five speeds, a second at each, and the last one
/// crosses a library in about a second. Starting slow is the point: most
/// scrolling is a few rows, and a control that leapt away on the first
/// repeat would overshoot every time.
///
/// Every list in the launcher scrolls through one of these -- the WHDLoad
/// games and favourites, the host disks -- and both ways of driving them,
/// a held arrow key and a held scroll-arrow button, go through the same
/// one, so they run at the same speed as each other.
///
/// Letting go and pressing again starts from the bottom: the speed is
/// measured from when the run of scrolling began, and a gap longer than a
/// repeat begins a new run.
#[derive(Debug, Clone, Default)]
pub struct ScrollRate {
    /// When the last movement was, to tell a continued scroll from a fresh
    /// one.
    last: Option<std::time::Instant>,
    /// When the run of scrolling this movement belongs to began, which is
    /// what the speed is worked out from.
    started: Option<std::time::Instant>,
}

impl ScrollRate {
    /// Rows a step, a stage a second. The last is what a long list needs
    /// and the first is what a short one does.
    const STAGES: [usize; 5] = [1, 3, 7, 14, 24];
    /// How long each stage lasts before the next takes over.
    const STAGE: std::time::Duration = std::time::Duration::from_secs(1);
    /// The gap after which a movement starts a new run rather than
    /// continuing one. Longer than the repeat of anything being held --
    /// the button repeat here, or a host keyboard's -- and shorter than a
    /// pause that means to stop.
    const CONTINUES_WITHIN: std::time::Duration = std::time::Duration::from_millis(250);

    /// How many rows this step should move, given when it arrived.
    pub fn rows_for_step(&mut self, now: std::time::Instant) -> usize {
        let continued = self
            .last
            .is_some_and(|last| now.duration_since(last) <= Self::CONTINUES_WITHIN);
        let started = match continued {
            true => self.started.unwrap_or(now),
            false => now,
        };
        self.started = Some(started);
        self.last = Some(now);
        let stage = now.duration_since(started).as_millis() / Self::STAGE.as_millis();
        Self::STAGES[(stage as usize).min(Self::STAGES.len() - 1)]
    }

    /// Back to the first stage, for a press that is deliberately a new one
    /// rather than the continuation of an old one.
    pub fn reset(&mut self) {
        *self = Self::default();
    }
}

/// The buckets the A-Z shortcut row offers, in the order it draws them.
///
/// Digits first, then everything that starts with neither a digit nor a
/// letter, then the alphabet. A game is filed by the first character of the
/// name the list shows for it, so what you click matches what you read.
#[cfg(feature = "game-library")]
pub const AZ_BUCKETS: usize = 28;

/// The label on bucket `at`.
#[cfg(feature = "game-library")]
pub fn az_label(at: usize) -> &'static str {
    const LETTERS: [&str; 26] = [
        "A", "B", "C", "D", "E", "F", "G", "H", "I", "J", "K", "L", "M", "N", "O", "P", "Q", "R",
        "S", "T", "U", "V", "W", "X", "Y", "Z",
    ];
    match at {
        0 => "0-9",
        1 => "#",
        _ => LETTERS.get(at - 2).copied().unwrap_or("#"),
    }
}

/// Which bucket a title belongs to.
///
/// The initial is folded to ASCII first, the same way the panel folds it
/// to draw it: "Élite" is drawn as "Elite" and sorted among the E's, so it
/// answers to E rather than sitting under `#` where nobody would look for
/// it.
#[cfg(feature = "game-library")]
pub fn az_bucket_of(title: &str) -> usize {
    let first = title.chars().next().map(|c| match c.is_ascii() {
        true => c,
        false => crate::video::font::fold(c).chars().next().unwrap_or(c),
    });
    match first {
        Some(c) if c.is_ascii_digit() => 0,
        Some(c) if c.is_ascii_alphabetic() => 2 + (c.to_ascii_uppercase() as usize - 'A' as usize),
        _ => 1,
    }
}

/// Which way a caret is being stepped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaretMove {
    Left,
    Right,
    Home,
    End,
}

/// The most the host box accepts: a DNS name is up to 253 characters.
/// Anything longer cannot be a host, so the box stops there.
const SERIAL_HOST_MAX: usize = 253;
/// The most the port box accepts: ports are five digits at the widest.
const SERIAL_PORT_MAX: usize = 5;

/// Why a typed serial address is not the `host:port` the TCP modes need,
/// or `None` when it is one.
///
/// Nothing here resolves a name or opens a socket: that happens at Run, and
/// a host that is merely down should not stop the address being typed. What
/// it does catch is the shape, so a missing or unreadable port is refused
/// while the box still has the focus to fix it in.
/// The two halves of a stored `host:port`, either absent. An IPv6 literal
/// is stored in brackets so only the colon after the closing one separates
/// the port; the brackets come off here and go back on in [`join_host_port`].
fn split_host_port(addr: &str) -> (Option<String>, Option<u16>) {
    if let Some(rest) = addr.strip_prefix('[') {
        // A bracketed IPv6 literal, with a port after the bracket or not.
        if let Some((host, tail)) = rest.split_once(']') {
            let port = tail.strip_prefix(':').and_then(|p| p.parse().ok());
            return ((!host.is_empty()).then(|| host.to_string()), port);
        }
    }
    match addr.rsplit_once(':') {
        Some((host, port)) => (
            (!host.is_empty()).then(|| host.to_string()),
            port.parse().ok(),
        ),
        None => ((!addr.is_empty()).then(|| addr.to_string()), None),
    }
}

/// A host in its stored spelling: brackets around anything a colon could
/// be mistaken inside.
fn bracket_host(host: &str) -> String {
    if host.contains(':') {
        format!("[{host}]")
    } else {
        host.to_string()
    }
}

/// The typed halves, stored exactly as typed -- a host alone stands alone,
/// a port alone keeps its leading colon -- so emptied halves revert and
/// an untouched pair stays out of the config. The absent halves are only
/// filled in (with [`crate::config::SERIAL_DEFAULT_HOST`] /
/// [`crate::config::SERIAL_DEFAULT_PORT`]) by [`complete_host_port`], on
/// the way to a saved config or a run.
fn join_host_port(host: Option<&str>, port: Option<u16>) -> Option<String> {
    match (host, port) {
        (None, None) => None,
        (Some(h), None) => Some(bracket_host(h)),
        (None, Some(p)) => Some(format!(":{p}")),
        (Some(h), Some(p)) => Some(format!("{}:{p}", bracket_host(h))),
    }
}

/// The address a run can use: the absent halves filled with their
/// defaults. `default_host` is the field's own -- loopback for the listen
/// addresses, and `None` for Connect, which has no host to assume: a
/// port-only Connect passes through partial and the run refuses it with
/// its own "needs a remote address" explanation rather than dialing a
/// host nobody named. An address the split cannot rebuild -- a
/// hand-written `host:notaport`, an unbracketed IPv6 literal -- passes
/// through untouched too, so it still fails loudly at Run instead of
/// silently binding a port the config never named.
fn complete_host_port(addr: &str, default_host: Option<&str>) -> String {
    let (host, port) = split_host_port(addr);
    if join_host_port(host.as_deref(), port).as_deref() != Some(addr) {
        return addr.to_string();
    }
    let Some(host) = host.or_else(|| default_host.map(str::to_string)) else {
        return addr.to_string();
    };
    let port = port.unwrap_or(crate::config::SERIAL_DEFAULT_PORT);
    format!("{}:{port}", bracket_host(&host))
}

/// [`complete_host_port`] with the listen addresses' loopback default.
fn complete_listen(addr: &str) -> String {
    complete_host_port(addr, Some(crate::config::SERIAL_DEFAULT_HOST))
}

/// [`complete_host_port`] for the dial-out address: no host is assumed.
fn complete_connect(addr: &str) -> String {
    complete_host_port(addr, None)
}

/// Whether this process may bind the privileged ports. Only asked on the
/// hosts that restrict them.
#[cfg(all(unix, feature = "midi"))]
fn can_bind_low_ports() -> bool {
    // SAFETY: geteuid takes nothing, returns the effective uid, cannot fail.
    unsafe { libc::geteuid() == 0 }
}

/// The largest size the box accepts, in whichever unit is showing.
/// The largest number the size box accepts: four digits, so 9999 GB is the
/// most that can be asked for. Past 2 TiB only an unpartitioned,
/// unformatted drive is possible -- see [`crate::diskimage::MAX_RDB_BYTES`].
const NEW_HARD_SIZE_MAX: u32 = 9999;

/// The unit the hard-drive size is typed in. Clicking it swaps to the
/// other, keeping the number: 8 MB becomes 8 GB.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum SizeUnit {
    #[default]
    Mb,
    Gb,
}

impl SizeUnit {
    pub fn label(self) -> &'static str {
        match self {
            SizeUnit::Mb => "MB",
            SizeUnit::Gb => "GB",
        }
    }

    fn bytes(self) -> u64 {
        match self {
            SizeUnit::Mb => 1 << 20,
            SizeUnit::Gb => 1 << 30,
        }
    }

    fn flipped(self) -> SizeUnit {
        match self {
            SizeUnit::Mb => SizeUnit::Gb,
            SizeUnit::Gb => SizeUnit::Mb,
        }
    }
}

/// What the next image will be made of.
///
/// Deliberately not part of [`MachineSetup`]: nothing here describes the
/// machine, so none of it belongs in a configuration file. It lives for as
/// long as the launcher is open and is thrown away with it.
#[derive(Debug, Clone)]
pub struct ImageWorkshop {
    pub density: crate::diskimage::Density,
    pub container: crate::diskimage::Container,
    /// `None` is an unformatted disk, for the guest to format itself.
    pub floppy_fs: Option<crate::diskimage::FileSystem>,
    pub floppy_label: String,
    pub floppy_bootable: bool,
    /// The size as typed, in [`ImageWorkshop::size_unit`].
    pub size: u32,
    pub size_unit: SizeUnit,
    /// Whether the geometry is the size's own or one set by hand.
    pub geometry_custom: bool,
    /// The geometry set by hand, once it has been. Kept while Auto is
    /// showing so going back to Custom finds it where it was left.
    pub custom_geometry: crate::diskimage::Geometry,
    pub partitioning: crate::diskimage::Partitioning,
    pub device: String,
    pub hard_fs: Option<crate::diskimage::FileSystem>,
    pub hard_label: String,
    pub hard_bootable: bool,
    pub boot_pri: i8,
    /// Blocks kept clear at the front of the partition.
    pub reserved: u32,
    /// What the drive says it is, per field, once that field has been
    /// told. A field left `None` -- the usual case -- keeps naming itself,
    /// so the Type follows the Size box instead of going stale behind it,
    /// and typing a Drive does not freeze the Type along with it.
    pub vendor: Option<String>,
    pub product: Option<String>,
    pub revision: Option<String>,
    pub read_only: bool,
    /// Leave the file's unwritten blocks as holes on the host. On by
    /// default: a sparse image is instant to make and costs only what it
    /// is actually used for.
    pub sparse: bool,
}

impl Default for ImageWorkshop {
    fn default() -> Self {
        Self {
            density: crate::diskimage::Density::Dd,
            container: crate::diskimage::Container::Adf,
            // A plain OFS floppy is the one that mounts on every Amiga
            // ever made, so it is where the page starts.
            floppy_fs: Some(crate::diskimage::FileSystem::OFS),
            floppy_label: "Empty".to_string(),
            floppy_bootable: false,
            size: 64,
            size_unit: SizeUnit::Mb,
            geometry_custom: false,
            custom_geometry: crate::diskimage::Geometry::for_size(64 << 20),
            partitioning: crate::diskimage::Partitioning::Rdb,
            device: "DH0".to_string(),
            hard_fs: Some(crate::diskimage::FileSystem::FFS),
            hard_label: "Work".to_string(),
            hard_bootable: true,
            boot_pri: 0,
            reserved: crate::diskimage::RESERVED_BLOCKS,
            vendor: None,
            product: None,
            revision: None,
            read_only: false,
            sparse: true,
        }
    }
}

impl ImageWorkshop {
    pub fn bytes(&self) -> u64 {
        u64::from(self.size.max(1)) * self.size_unit.bytes()
    }

    /// Swap MB and GB, keeping the number: 8 MB becomes 8 GB.
    pub fn flip_size_unit(&mut self) {
        self.size_unit = self.size_unit.flipped();
    }

    /// The geometry the image will carry: the one set by hand, or the one
    /// the size implies.
    pub fn effective_geometry(&self) -> crate::diskimage::Geometry {
        if self.geometry_custom {
            self.custom_geometry
        } else {
            crate::diskimage::Geometry::for_size(self.bytes())
        }
    }

    /// Fill the custom geometry in from the size, and put the drive's
    /// identity back to naming itself. This is what the editor's Auto
    /// button does: everything on the page returns to what Copperline
    /// would have chosen.
    pub fn geometry_from_size(&mut self) {
        self.custom_geometry = crate::diskimage::Geometry::for_size(self.bytes());
        self.vendor = None;
        self.product = None;
        self.revision = None;
    }

    /// What the drive will say it is: each field as typed, or as the drive
    /// names itself. Resolved on the way out rather than stored, so a field
    /// nobody has touched still follows the size.
    pub fn identity(&self) -> crate::harddrive::RdbIdentity {
        let named = crate::harddrive::default_rdb_identity(self.effective_geometry().bytes());
        crate::harddrive::RdbIdentity {
            vendor: self.vendor.clone().unwrap_or(named.vendor),
            product: self.product.clone().unwrap_or(named.product),
            revision: self.revision.clone().unwrap_or(named.revision),
        }
    }

    /// Take one identity field as typed, leaving the others naming
    /// themselves: editing the Drive does not freeze the Type at whatever
    /// size happened to be showing.
    pub fn set_identity_field(&mut self, field: LauncherField, text: String) {
        match field {
            F::NewGeomVendor => self.vendor = Some(text),
            F::NewGeomProduct => self.product = Some(text),
            F::NewGeomRevision => self.revision = Some(text),
            _ => {}
        }
    }

    pub fn floppy_spec(&self) -> crate::diskimage::FloppySpec {
        crate::diskimage::FloppySpec {
            density: self.density,
            container: self.container,
            filesystem: self.floppy_fs,
            bootable: self.floppy_bootable && self.floppy_fs.is_some(),
            label: self.floppy_label.clone(),
        }
    }

    pub fn hard_spec(&self) -> crate::diskimage::HardSpec {
        crate::diskimage::HardSpec {
            bytes: self.bytes(),
            geometry: self.geometry_custom.then_some(self.custom_geometry),
            partitioning: self.partitioning,
            filesystem: self.hard_fs,
            device: self.device.clone(),
            label: self.hard_label.clone(),
            // The boot flag and its rank live in the partition entry, so
            // without a partition table there is nothing to carry them:
            // the spec says what will happen, not what the page shows.
            bootable: self.hard_bootable
                && self.partitioning == crate::diskimage::Partitioning::Rdb,
            boot_pri: self.boot_pri,
            reserved: self.reserved,
            identity: Some(self.identity()),
            read_only: self.read_only,
            sparse: self.sparse,
        }
    }

    /// A file name to offer in the save dialog, from what is being made.
    pub fn suggested_name(&self, floppy: bool) -> String {
        let stem = |s: &str| {
            let cleaned: String = s
                .chars()
                .filter(|c| c.is_ascii_alphanumeric())
                .collect::<String>();
            if cleaned.is_empty() {
                "image".to_string()
            } else {
                cleaned
            }
        };
        if floppy {
            format!("{}.adf", stem(&self.floppy_label))
        } else {
            format!("{}.hdf", stem(&self.hard_label))
        }
    }
}

/// How many characters a workshop text field takes, or `None` when it is
/// not one of the fixed-width ones.
///
/// The identity boxes are exactly as wide as the Rigid Disk Block's fields,
/// because a longer string would not be cut so much as spill into the next
/// field -- which is the bug that made a drive called `Copperli` of type
/// `ne`. Stopping the typing at the width is the honest place to stop it.
fn workshop_text_limit(field: LauncherField) -> Option<usize> {
    let widths = crate::harddrive::RDB_IDENTITY_WIDTHS;
    match field {
        F::NewGeomVendor => Some(widths[0]),
        F::NewGeomProduct => Some(widths[1]),
        F::NewGeomRevision => Some(widths[2]),
        _ => None,
    }
}

/// How many characters a workshop number field takes, or `None` when the
/// field is a word rather than a number.
fn workshop_digit_limit(field: LauncherField) -> Option<usize> {
    match field {
        // 9999 is the largest size the box accepts.
        F::NewHardSize => Some(4),
        // -128..=127.
        F::NewHardBootPri => Some(4),
        F::NewGeomCylinders | F::NewGeomSurfaces | F::NewGeomSectors => Some(5),
        F::NewGeomReserved => Some(3),
        _ => None,
    }
}

/// Nudge a geometry figure by one, keeping it inside `floor..=ceiling`.
fn step_u32(value: u32, forward: bool, floor: u32, ceiling: u32) -> u32 {
    if forward {
        value.saturating_add(1).min(ceiling)
    } else {
        value.saturating_sub(1).max(floor)
    }
}

/// The largest value a workshop number box will hold, from the digits it
/// accepts: the arrows and the keyboard have to agree on the same range.
fn workshop_ceiling(field: LauncherField) -> u32 {
    match workshop_digit_limit(field) {
        Some(digits) => 10u32.saturating_pow(digits as u32) - 1,
        None => u32::MAX,
    }
}

/// Every filesystem the pickers offer, unformatted first.
/// What the picker's first row offers, in the order it draws them.
/// `Unformatted` is a real choice, not an absent one: the volume is left
/// for the Amiga to format itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FsFamily {
    Unformatted,
    Ofs,
    Ffs,
}

impl FsFamily {
    pub const ALL: [FsFamily; 3] = [FsFamily::Unformatted, FsFamily::Ofs, FsFamily::Ffs];

    pub fn label(self) -> &'static str {
        match self {
            FsFamily::Unformatted => "Unformatted",
            FsFamily::Ofs => "OFS",
            FsFamily::Ffs => "FFS",
        }
    }

    /// Whether the DOS type carries identifiers to choose from, which an
    /// unformatted volume has no room for -- it has no boot block to tag.
    pub fn has_identifiers(self) -> bool {
        matches!(self, FsFamily::Ofs | FsFamily::Ffs)
    }

    /// The family a chosen filesystem belongs to.
    pub fn of(fs: Option<crate::diskimage::FileSystem>) -> FsFamily {
        match fs {
            None => FsFamily::Unformatted,
            Some(fs) if fs.ffs => FsFamily::Ffs,
            Some(_) => FsFamily::Ofs,
        }
    }
}

/// What a ROM row's chosen image turned out to be, remembered against the
/// path it was read from. Identification opens and checksums the file, which
/// a redraw must never do, so it happens only when the path in the field
/// changes (see [`LauncherState::sync_rom_notes`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct RomNote {
    path: Option<PathBuf>,
    text: Option<String>,
    /// Whether the slot has been synced at all: the bundled default
    /// (path None) must still seed its cells once.
    seeded: bool,
    /// The identification split into its Name, Version and Revision
    /// facts, computed when the path changes rather than per draw (the
    /// bundled AROS reads its numbers off the image file).
    cells: (String, String, String),
}

/// The full interactive state of the open configuration panel.
#[derive(Debug, Clone)]
pub struct LauncherState {
    pub setup: MachineSetup,
    /// Whether the Save dialog is up: Save As, Set default, Reset default.
    /// One flag, because it holds nothing -- three buttons and a close
    /// gadget, and every click either picks one or puts it away.
    pub save_dialog: bool,
    /// Whether the "are you sure" over Reset default is up. Only ever set
    /// when there is a default to delete -- with none saved there is
    /// nothing to be sure about, and a dialog asking anyway is a dialog
    /// that teaches people to dismiss dialogs.
    pub confirm_reset: bool,
    /// What the Create Image pages will make. Not machine configuration, so
    /// it sits beside the setup rather than inside it.
    pub workshop: ImageWorkshop,
    pub netplay: NetplaySetup,
    pub tab: LauncherTab,
    pub status: Option<StatusMessage>,
    /// The Kickstart / extended ROM identifications shown on the ROM tab,
    /// in that order.
    rom_notes: [RomNote; 2],
    /// The text field being typed into, and the edit buffer, when one has
    /// focus (a plugin string option or a drive volume name).
    editing: Option<EditTarget>,
    edit_buffer: String,
    /// Where typing goes in `edit_buffer`.
    edit_caret: Caret,
    /// The Library page: the games found, which is chosen, and where the
    /// list is scrolled to. Held here rather than in the setup for the
    /// same reason the workshop is -- none of it describes a machine.
    #[cfg(feature = "game-library")]
    pub library: LibraryPage,
    /// The signed-in OpenRetro session, for as long as the launcher is
    /// open. Shared with a running scan rather than handed over, so the
    /// row can still say it is signed in while one is going.
    #[cfg(feature = "game-library")]
    pub openretro: Option<std::sync::Arc<crate::gamelib::openretro::Session>>,
    /// The sign-in dialog, when it is up.
    #[cfg(feature = "game-library")]
    pub login: Option<LoginDialog>,
    /// The metadata editor, when it is up.
    #[cfg(feature = "game-library")]
    pub meta: Option<MetaDialog>,
}

/// What the Library page is showing.
#[cfg(feature = "game-library")]
#[derive(Debug, Default, Clone)]
pub struct LibraryPage {
    /// The game database, read once when the page is first opened.
    pub db: crate::gamelib::Database,
    /// Whether the database has been read yet, so an empty one is not
    /// re-read on every frame.
    pub db_loaded: bool,
    /// The games found beside the chosen one.
    pub games: crate::gamelib::Library,
    /// Which of the two lists the keyboard is walking, and which row is
    /// chosen in it. Only the focused list draws a chosen row: a game
    /// highlighted in both at once reads as two selections.
    pub focus: LibraryFocus,
    /// Which entry is chosen, as an index into `games`.
    pub selected: usize,
    /// Which row of the favourites list is chosen, as a position in it.
    pub favourite_selected: usize,
    /// The first row drawn, for scrolling.
    pub scroll: usize,
    /// The same for the favourites list, which scrolls on its own: a
    /// collection can be favourited past the handful of rows the box shows.
    pub favourite_scroll: usize,
    /// How fast a continued scroll is running. One per list, so leaving one
    /// part-way through and picking up the other starts the other at a row
    /// a notch rather than at whatever speed the first had reached.
    pub scroll_rate: ScrollRate,
    pub favourite_scroll_rate: ScrollRate,
    /// Cover art, fetched for whatever is selected and kept afterwards.
    pub covers: crate::gamelib::Covers,
}

/// A package's own name, without the folders above it or its extension.
#[cfg(feature = "game-library")]
fn file_stem_of(relative: &str) -> &str {
    let base = relative.rsplit(['/', '\\']).next().unwrap_or(relative);
    base.rsplit_once('.').map(|(stem, _)| stem).unwrap_or(base)
}

/// Which of the Library page's two lists is being walked.
#[cfg(feature = "game-library")]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum LibraryFocus {
    #[default]
    Games,
    Favourites,
}

/// One field of the metadata editor.
#[cfg(feature = "game-library")]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum MetaField {
    #[default]
    Name,
    Year,
    Publisher,
    Developer,
    Players,
    Version,
}

#[cfg(feature = "game-library")]
impl MetaField {
    pub const ALL: [MetaField; 6] = [
        MetaField::Name,
        MetaField::Year,
        MetaField::Publisher,
        MetaField::Developer,
        MetaField::Players,
        MetaField::Version,
    ];

    pub fn label(self) -> &'static str {
        match self {
            MetaField::Name => "Name",
            MetaField::Year => "Year",
            MetaField::Publisher => "Publisher",
            MetaField::Developer => "Developer",
            MetaField::Players => "Players",
            MetaField::Version => "Version",
        }
    }

    fn at(self) -> usize {
        MetaField::ALL.iter().position(|&f| f == self).unwrap_or(0)
    }
}

/// The metadata editor, while it is open.
///
/// Everything in it is a copy: nothing reaches the store until Save, so
/// Cancel really is "leave it as it was", and Clear is something you can
/// change your mind about.
#[cfg(feature = "game-library")]
#[derive(Debug, Clone, Default)]
pub struct MetaDialog {
    /// The package being edited, by the name the store files it under.
    pub file: String,
    /// The values, in [`MetaField::ALL`] order.
    pub values: [String; 6],
    /// The cache key of the art, which is a catalogue digest for art that
    /// was downloaded and a `manual-` name for art somebody chose.
    pub art: Option<String>,
    pub focus: MetaField,
    /// Where typing goes in the focused field. Metadata is more often
    /// amended than typed fresh, so being able to step into what is already
    /// there is most of the point of the editor.
    pub caret: Caret,
}

#[cfg(feature = "game-library")]
impl MetaDialog {
    pub fn value(&self, field: MetaField) -> &str {
        &self.values[field.at()]
    }

    pub fn value_mut(&mut self, field: MetaField) -> &mut String {
        &mut self.values[field.at()]
    }

    /// Move the focus to another box, putting the caret at the end of what
    /// that one holds.
    pub fn focus_on(&mut self, field: MetaField) {
        self.focus = field;
        self.caret = Caret::end_of(self.value(field));
    }

    /// Type into the focused box at the caret, up to what it accepts.
    pub fn insert(&mut self, c: char, most: usize) {
        if self.value(self.focus).chars().count() >= most {
            return;
        }
        let focus = self.focus;
        let mut caret = self.caret;
        caret.insert(self.value_mut(focus), c);
        self.caret = caret;
    }

    /// Step the caret through the focused box.
    pub fn caret_move(&mut self, to: CaretMove) {
        let len = self.value(self.focus).chars().count();
        match to {
            CaretMove::Left => self.caret.left(),
            CaretMove::Right => self.caret.right_of(len),
            CaretMove::Home => self.caret.home(),
            CaretMove::End => self.caret.end_of_len(len),
        }
    }

    /// Whether there is anything left to save. An editor emptied and saved
    /// hands the package back to the scan rather than pinning it empty.
    pub fn is_empty(&self) -> bool {
        self.art.is_none() && self.values.iter().all(|v| v.trim().is_empty())
    }
}

/// Which box of the sign-in dialog is being typed into.
#[cfg(feature = "game-library")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoginField {
    User,
    Pass,
}

/// The sign-in dialog, while it is open.
///
/// Nothing here is kept: the password is traded for a token when OK is
/// pressed and wiped on the way out, and the token lives no longer than the
/// session. See [`crate::gamelib::Secret`] for what that costs to do
/// properly.
#[cfg(feature = "game-library")]
#[derive(Debug)]
pub struct LoginDialog {
    pub user: String,
    pub pass: crate::gamelib::Secret,
    pub focus: LoginField,
    /// Where typing goes in the focused box, as in the metadata editor: one
    /// dialog behaving differently from the other is one more thing to
    /// learn. It steps over the mask in the password box, which is what the
    /// mask is drawn a character at a time for.
    pub caret: Caret,
    /// Set while the sign-in request is in flight, so a second Return does
    /// not start a second one.
    pub sending: bool,
}

/// A cloned launcher state does not carry a half-typed password. That is
/// the only sensible thing a copy of a credential could be, and it is why
/// this is written rather than derived.
#[cfg(feature = "game-library")]
impl Clone for LoginDialog {
    fn clone(&self) -> LoginDialog {
        LoginDialog::default()
    }
}

#[cfg(feature = "game-library")]
impl Default for LoginDialog {
    fn default() -> LoginDialog {
        LoginDialog {
            user: String::new(),
            pass: crate::gamelib::Secret::new(),
            focus: LoginField::User,
            caret: Caret::default(),
            sending: false,
        }
    }
}

#[cfg(feature = "game-library")]
impl LoginDialog {
    /// How many characters the focused box holds. The password is counted
    /// rather than read: the caret needs its length, not its text.
    fn focused_len(&self) -> usize {
        match self.focus {
            LoginField::User => self.user.chars().count(),
            LoginField::Pass => self.pass.chars(),
        }
    }

    /// Move to the other box, with the caret at the end of what it holds.
    pub fn focus_on(&mut self, field: LoginField) {
        self.focus = field;
        self.caret = Caret::at_end_of_len(self.focused_len());
    }

    /// Type at the caret, up to what the box takes.
    pub fn insert(&mut self, c: char) {
        match self.focus {
            LoginField::User if self.user.chars().count() < USER_MAX => {
                let mut caret = self.caret;
                caret.insert(&mut self.user, c);
                self.caret = caret;
            }
            // Full: the character is dropped and the caret stays put.
            LoginField::User => {}
            // The Secret bounds itself, and says nothing about how full it
            // is, so the caret follows only a character that went in.
            LoginField::Pass => {
                let was = self.pass.chars();
                self.pass.insert_at(self.caret.at(), c);
                if self.pass.chars() == was {
                    return;
                }
                self.caret.right_of(self.pass.chars());
            }
        }
    }

    /// Delete backwards from the caret.
    pub fn backspace(&mut self) {
        if self.caret.at() == 0 {
            return;
        }
        match self.focus {
            LoginField::User => {
                let mut caret = self.caret;
                caret.backspace(&mut self.user);
                self.caret = caret;
            }
            LoginField::Pass => {
                self.caret.left();
                self.pass.remove_at(self.caret.at());
            }
        }
    }

    /// Delete the character the caret is on.
    pub fn delete(&mut self) {
        match self.focus {
            LoginField::User => {
                let mut caret = self.caret;
                caret.delete(&mut self.user);
                self.caret = caret;
            }
            LoginField::Pass => {
                self.pass.remove_at(self.caret.at());
            }
        }
    }

    /// Step the caret through the focused box.
    pub fn caret_move(&mut self, to: CaretMove) {
        let len = self.focused_len();
        match to {
            CaretMove::Left => self.caret.left(),
            CaretMove::Right => self.caret.right_of(len),
            CaretMove::Home => self.caret.home(),
            CaretMove::End => self.caret.end_of_len(len),
        }
    }
}

/// The longest an OpenRetro user name is taken to be. Nothing documents a
/// limit; this is a box, not a validator, and a name past it is a paste
/// that went somewhere it did not belong.
#[cfg(feature = "game-library")]
const USER_MAX: usize = 64;

/// The support directory under a given configuration directory, from
/// `paths` so this and the no-argument helpers cannot describe different
/// trees. Kept as a name here because the call sites read better for it.
#[cfg(feature = "game-library")]
fn whdload_support_under(config_dir: &std::path::Path) -> PathBuf {
    crate::paths::whdload_support_in(config_dir)
}

#[cfg(feature = "game-library")]
impl MachineSetup {
    /// Where the scanned library is kept: `[whdload] library_db`, or the
    /// default under the configuration directory.
    pub fn library_db(&self, config_dir: &std::path::Path) -> PathBuf {
        self.whdload_library_db
            .clone()
            .unwrap_or_else(|| whdload_support_under(config_dir).join("launcher.db"))
    }

    /// Where a scan keeps what it downloaded: `[whdload] library_cache`, or
    /// the default beside the library it belongs to.
    pub fn library_cache(&self, config_dir: &std::path::Path) -> PathBuf {
        self.whdload_library_cache
            .clone()
            .unwrap_or_else(|| whdload_support_under(config_dir).join("cache"))
    }

    /// Adopt whatever is already sitting where WHDLoad would have put it.
    ///
    /// A person who has run the packaging script, or downloaded once and
    /// then started a fresh configuration, should not have to point at the
    /// same files again. Only fills a setting that is empty, so a chosen
    /// path is never quietly replaced -- and only adopts an archive whose
    /// digest is right, since a half-finished download in the right place
    /// with the right name is exactly what should *not* be adopted.
    pub fn adopt_whdload_defaults(&mut self) {
        for archive in crate::gamelib::support::Archive::ALL {
            let field = match archive {
                crate::gamelib::support::Archive::Whdload => F::WhdloadWhdPackage,
                crate::gamelib::support::Archive::Skick => F::WhdloadSkickPackage,
            };
            if self.path(field).is_none() {
                if let Some(at) = archive.found_locally() {
                    self.set_path(field, at);
                }
            }
        }
        // Kickstart images are somebody's own collection rather than a
        // known file, so the test is that the directory has anything in it
        // at all.
        if self.path(F::WhdloadKickstarts).is_none() {
            if let Some(dir) = crate::gamelib::support::default_kickstart_dir() {
                if crate::gamelib::support::holds_anything(&dir) {
                    self.set_path(F::WhdloadKickstarts, dir);
                }
            }
        }
    }
}

#[cfg(feature = "game-library")]
impl LauncherState {
    /// Bring the Library page up to date with what the configuration says.
    ///
    /// Called when the page is drawn or clicked rather than when the
    /// settings change: the game and the database path are ordinary path
    /// fields that anything may set -- a Browse, a drop, a loaded config --
    /// and one place that notices beats a dozen that have to remember to.
    pub fn refresh_library(&mut self, config_dir: &std::path::Path) {
        // Turned off, the page does nothing at all: no store read, no
        // cover worker started, nothing held. Whatever a previous session
        // left in memory goes with it.
        if !self.setup.whdload_enabled() {
            if self.library.db_loaded {
                self.library = LibraryPage::default();
            }
            return;
        }
        // No game library means nothing to list -- and nothing to show in
        // the favourites either. They are kept in the store, so without
        // this a fresh install with no library set still listed whatever a
        // previous one had starred.
        if self.library_folder().is_none() {
            if self.library.db_loaded {
                self.library = LibraryPage::default();
            }
            return;
        }
        if !self.library.db_loaded {
            self.library.db = crate::gamelib::Database::load(&self.setup.library_db(config_dir));
            // The covers subdirectory of the cache, not the cache itself:
            // that is where a scan writes them.
            self.library.covers = crate::gamelib::Covers::new(crate::gamelib::scan::covers_path(
                &self.setup.library_cache(config_dir),
            ));
            self.library.db_loaded = true;
        }
        // From the store, not from the disk: opening a page is not a
        // reason to walk a collection of several thousand packages. The
        // Refresh button is.
        match self.library_folder() {
            // A store built from another folder is not this folder's list:
            // its entries are paths under somewhere else, and showing them
            // here offers games that are not there. Refresh is what reads
            // the folder and makes the store this one's.
            Some(folder) if !self.library.db.lists(&folder) => {
                self.library.games = crate::gamelib::Library::default();
            }
            Some(folder) if !self.library.games.covers(&folder) => {
                self.library.games = crate::gamelib::Library::known(&folder, &self.library.db);
                self.select_running_game();
            }
            None => self.library.games = crate::gamelib::Library::default(),
            _ => {}
        }
        // A list that shrank under the selection must not leave it past the
        // end, where nothing would be drawn as chosen.
        self.library.selected = self
            .library
            .selected
            .min(self.library.games.len().saturating_sub(1));
    }

    /// Put the selection on the game the configuration names, so opening
    /// the page while one is running shows which.
    fn select_running_game(&mut self) {
        if let Some(at) = self
            .setup
            .path(F::WhdloadGame)
            .and_then(|game| self.library.games.position(game))
        {
            self.library.selected = at;
        }
    }

    /// The entry the list is on, if the list has any.
    /// Keep the cover art up with the selection: collect anything that has
    /// arrived, and ask for what is being looked at now. Answers whether
    /// the page changed and should be drawn again.
    ///
    /// Here rather than at each place a game is chosen, because every route
    /// to a selection -- a click, an arrow key, the favourites list, a
    /// config that named a game -- has to end up asking.
    pub fn poll_library_covers(&mut self) -> bool {
        let arrived = self.library.covers.poll();
        // The selection and the few either side of it. Reading ahead is
        // what makes a steady scroll find each cover already decoded
        // instead of waiting for it at every step.
        let at = self.library.selected;
        let entries = self.library.games.entries();
        let window: Vec<Option<&str>> = entries
            .iter()
            .map(|entry| {
                entry
                    .game
                    .as_ref()
                    .and_then(|game| game.front_sha1.as_deref())
            })
            .collect();
        self.library.covers.want_around(window.into_iter(), at);
        // And whatever the editor is showing, which after a picture has
        // been chosen is not the selection's art any more.
        if let Some(key) = self.meta.as_ref().and_then(|meta| meta.art.clone()) {
            self.library.covers.want(&key);
        }
        arrived
    }

    /// The folder the list is of.
    ///
    /// A game library is a collection and is searched all the way down;
    /// without one, the folder holding the chosen game stands in, so
    /// pointing at any package still lists its neighbours. The library
    /// wins where both are set: it is the deliberate answer to "where are
    /// my games", and the other is a guess from one of them.
    /// The folder the Library page lists, which is the one that was set
    /// and nothing else.
    ///
    /// It used to fall back to the folder holding the chosen game, so that
    /// pointing at a package listed its neighbours. That made Clear look
    /// broken -- emptying the setting left the list full of whatever sat
    /// beside the launch game -- and it hid the one thing the empty page
    /// has to say, which is where to set the folder.
    pub fn library_folder(&self) -> Option<PathBuf> {
        self.setup
            .path(F::WhdloadGames)
            .map(std::path::Path::to_path_buf)
    }

    pub fn library_selection(&self) -> Option<&crate::gamelib::Entry> {
        match self.library.focus {
            LibraryFocus::Games => self.library.games.entries().get(self.library.selected),
            // A favourite whose package is no longer there has no entry,
            // and so nothing to show beside it.
            LibraryFocus::Favourites => {
                let key = self.favourite_key(self.library.favourite_selected)?;
                self.library
                    .games
                    .entries()
                    .iter()
                    .find(|entry| entry.relative == key)
            }
        }
    }

    /// The key of the favourite on that row.
    pub fn favourite_key(&self, drawn: usize) -> Option<&str> {
        self.library.db.favourites().nth(drawn).map(|(key, _)| key)
    }

    /// Choose a game, which is also to say launch it when Run is pressed:
    /// the selection *is* the configured game rather than something that
    /// has to be copied across on the way out.
    pub fn select_library_game(&mut self, index: usize) {
        let Some(entry) = self.library.games.entries().get(index) else {
            return;
        };
        let path = entry.path.clone();
        self.library.focus = LibraryFocus::Games;
        self.library.selected = index;
        self.setup.set_path(F::WhdloadGame, path);
        self.status = None;
    }

    /// Mark or unmark the game at `index` in the list.
    pub fn toggle_library_favourite(&mut self, index: usize) {
        let Some(entry) = self.library.games.entries().get(index) else {
            return;
        };
        // The name is kept with the mark, so the favourite still reads as
        // a game once its package is gone.
        // The package, not the game: a collection holds the same game
        // several times over, and starring one of them stars that one.
        let (file, title) = (entry.relative.clone(), entry.title().to_string());
        self.library.db.toggle_favourite(&file, &title);
    }

    /// Choose a favourite: the game to launch, without also being the row
    /// highlighted in the list above. Two highlights for one choice is
    /// two choices as far as anyone reading the page is concerned.
    pub fn select_favourite(&mut self, drawn: usize) {
        let Some(key) = self.favourite_key(drawn).map(str::to_string) else {
            return;
        };
        self.library.focus = LibraryFocus::Favourites;
        self.library.favourite_selected = drawn;
        self.status = None;
        // A favourite whose package has been deleted is still a row worth
        // landing on -- its Remove tick is the point of it -- but there is
        // nothing to launch.
        let path = self
            .library
            .games
            .entries()
            .iter()
            .find(|entry| entry.relative == key)
            .map(|entry| entry.path.clone());
        if let Some(path) = path {
            self.setup.set_path(F::WhdloadGame, path);
        }
    }

    /// Open the metadata editor on whatever is selected.
    pub fn open_meta_editor(&mut self) -> bool {
        let Some(entry) = self.library_selection() else {
            return false;
        };
        // The store keys on the path under the game folder; the version
        // offered below is the package's own name, which is what tells one
        // release from another. Without its extension -- `.lha` against
        // `.zip` is how it was packed, not which release it is.
        let file = entry.relative.clone();
        let base = entry.file_name.clone();
        let duplicated = entry.duplicated;
        let game = entry.game.clone();
        let mut dialog = MetaDialog {
            file,
            focus: MetaField::Name,
            art: game.as_ref().and_then(|g| g.front_sha1.clone()),
            ..Default::default()
        };
        if let Some(game) = &game {
            for (field, value) in [
                (MetaField::Name, Some(game.name.clone())),
                (MetaField::Year, game.year.clone()),
                (MetaField::Publisher, game.publisher.clone()),
                (MetaField::Developer, game.developer.clone()),
                (MetaField::Players, game.players.clone()),
                (MetaField::Version, game.version.clone()),
            ] {
                *dialog.value_mut(field) = value.unwrap_or_default();
            }
        }
        // Nothing stored, the game has metadata, and the library holds it
        // more than once: offer the file name, which is the only thing that
        // separates them. Blank for a game held once, and blank for one the
        // scan could not name at all -- filling in a version for a package
        // with nothing else known about it says nothing.
        if dialog.value(MetaField::Version).is_empty() && duplicated && game.is_some() {
            *dialog.value_mut(MetaField::Version) = base;
        }
        // The caret goes to the end of the first box, which is where typing
        // would go anyway and where the block is expected to be.
        dialog.focus_on(MetaField::Name);
        self.meta = Some(dialog);
        self.status = None;
        true
    }

    /// Commit the editor into the store, and answer where to save it.
    ///
    /// An editor with nothing in it clears the entry and hands it back to
    /// the scan; anything else is marked as hand-filled, which is what
    /// stops the next scan overwriting it.
    pub fn commit_meta_editor(&mut self) {
        let Some(dialog) = self.meta.take() else {
            return;
        };
        // Keeping the digest means a later scan still recognises the
        // package without opening it again.
        let held_digest = self
            .library
            .db
            .entry(&dialog.file)
            .and_then(|k| k.slave_sha1.clone());
        let field = |f: MetaField| {
            let value = dialog.value(f).trim();
            (!value.is_empty()).then(|| value.to_string())
        };
        let entry = if dialog.is_empty() {
            crate::gamelib::Known {
                file: dialog.file.clone(),
                game: None,
                slave_sha1: held_digest,
                manual: false,
            }
        } else {
            crate::gamelib::Known {
                file: dialog.file.clone(),
                game: Some(crate::gamelib::Game {
                    // Kept, so a re-sync can still recognise the record
                    // this started life as.
                    uuid: self
                        .library
                        .db
                        .entry(&dialog.file)
                        .and_then(|k| k.game.as_ref())
                        .map(|g| g.uuid.clone())
                        .unwrap_or_default(),
                    // A name left blank falls back to what the list would
                    // have shown anyway -- the file's own name, without
                    // the folders it sits in or its extension.
                    name: field(MetaField::Name)
                        .unwrap_or_else(|| file_stem_of(&dialog.file).to_string()),
                    year: field(MetaField::Year),
                    publisher: field(MetaField::Publisher),
                    developer: field(MetaField::Developer),
                    players: field(MetaField::Players),
                    version: field(MetaField::Version),
                    front_sha1: dialog.art.clone(),
                }),
                slave_sha1: held_digest,
                manual: true,
            }
        };
        self.library.db.set_entry(entry);
    }

    /// Move the selection in whichever list is focused.
    pub fn step_library_focus(&mut self, delta: isize, visible: usize) {
        match self.library.focus {
            LibraryFocus::Games => self.step_library(delta, visible),
            LibraryFocus::Favourites => {
                let len = self.library.db.favourite_count();
                if len == 0 {
                    return;
                }
                let at = (self.library.favourite_selected as isize + delta)
                    .clamp(0, len as isize - 1) as usize;
                self.select_favourite(at);
                self.scroll_favourites_into_view(visible);
            }
        }
    }

    /// Take a favourite off from the favourites list itself.
    pub fn remove_favourite(&mut self, at: usize) {
        if let Some(key) = self.favourite_key(at).map(str::to_string) {
            self.library.db.remove_favourite(&key);
        }
        // The list just got shorter under both the selection and the scroll,
        // either of which can now be off the end of it.
        self.clamp_favourites();
    }

    /// Keep the favourites selection and scroll inside a list that may have
    /// shrunk -- a removal here, or a database re-read that dropped some.
    fn clamp_favourites(&mut self) {
        let last = self.library.db.favourite_count().saturating_sub(1);
        self.library.favourite_selected = self.library.favourite_selected.min(last);
        self.library.favourite_scroll = self.library.favourite_scroll.min(last);
    }

    /// Re-read the game folder: the one thing that walks the disk, and the
    /// only way a package that was not there before gets into the store.
    ///
    /// What it finds goes into the store rather than only into the list, so
    /// the page shows the same thing when it is next opened and after a
    /// restart. Metadata already resolved is carried across -- reading the
    /// folder again must not be a way to lose the work a scan did.
    ///
    /// Answers how many packages are in the folder, and how many of those
    /// have nothing known about them yet.
    pub fn rescan_library(&mut self, config_dir: &std::path::Path) -> (usize, usize) {
        self.refresh_library(config_dir);
        let Some(folder) = self.library_folder() else {
            return (0, 0);
        };
        let fresh = self
            .library
            .db
            .merge_found(crate::gamelib::scan::packages(&folder));
        self.library.db.set_folder(&folder);
        self.save_library_database(config_dir);
        self.library.games = crate::gamelib::Library::known(&folder, &self.library.db);
        self.select_running_game();
        self.library.selected = self
            .library
            .selected
            .min(self.library.games.len().saturating_sub(1));
        (self.library.games.len(), fresh)
    }

    /// Write the database out, so a favourite outlives the session.
    pub fn save_library_database(&self, config_dir: &std::path::Path) {
        let at = self.setup.library_db(config_dir);
        if let Err(e) = self.library.db.save(&at) {
            log::warn!("game library: could not save {}: {e}", at.display());
        }
    }

    /// Move the selection by `delta` rows, keeping it in view.
    pub fn step_library(&mut self, delta: isize, visible: usize) {
        let len = self.library.games.len();
        if len == 0 {
            return;
        }
        let at = (self.library.selected as isize + delta).clamp(0, len as isize - 1) as usize;
        self.select_library_game(at);
        self.scroll_library_into_view(visible);
    }

    /// Scroll the list by `delta` rows without moving the selection.
    pub fn scroll_library(&mut self, delta: isize, visible: usize) {
        let last_start = self.library.games.len().saturating_sub(visible);
        let at = self.library.scroll as isize + delta;
        self.library.scroll = at.clamp(0, last_start as isize) as usize;
    }

    /// Which buckets the list has games in, so the row can grey the rest.
    pub fn az_buckets_present(&self) -> [bool; AZ_BUCKETS] {
        let mut present = [false; AZ_BUCKETS];
        for entry in self.library.games.entries() {
            if let Some(slot) = present.get_mut(az_bucket_of(entry.title())) {
                *slot = true;
            }
        }
        present
    }

    /// Jump the list to the first game in a bucket, and choose it.
    pub fn jump_to_bucket(&mut self, bucket: usize, visible: usize) {
        let Some(at) = self
            .library
            .games
            .entries()
            .iter()
            .position(|entry| az_bucket_of(entry.title()) == bucket)
        else {
            return;
        };
        self.select_library_game(at);
        // The letter's first game at the top of the box rather than merely
        // on screen: the point of the jump is to see what is under it.
        let last_start = self.library.games.len().saturating_sub(visible);
        self.library.scroll = at.min(last_start);
    }

    pub fn scroll_favourites(&mut self, delta: isize, visible: usize) {
        let last_start = self.library.db.favourite_count().saturating_sub(visible);
        let at = self.library.favourite_scroll as isize + delta;
        self.library.favourite_scroll = at.clamp(0, last_start as isize) as usize;
    }

    fn scroll_favourites_into_view(&mut self, visible: usize) {
        let at = self.library.favourite_selected;
        if at < self.library.favourite_scroll {
            self.library.favourite_scroll = at;
        } else if visible > 0 && at >= self.library.favourite_scroll + visible {
            self.library.favourite_scroll = at + 1 - visible;
        }
    }

    /// Bring the selection into the drawn rows, moving as little as will do.
    fn scroll_library_into_view(&mut self, visible: usize) {
        let at = self.library.selected;
        if at < self.library.scroll {
            self.library.scroll = at;
        } else if visible > 0 && at >= self.library.scroll + visible {
            self.library.scroll = at + 1 - visible;
        }
    }
}

impl LauncherState {
    /// Whether a field belongs to the Create Image workshop rather than to
    /// the machine, so the drawing and click paths read the right state.
    pub fn is_workshop(field: LauncherField) -> bool {
        matches!(
            field,
            F::NewFloppyDensity
                | F::NewFloppyContainer
                | F::NewFloppyFs
                | F::NewFloppyFsVariant
                | F::NewFloppyLabel
                | F::NewFloppyBootable
                | F::NewFloppyCreate
                | F::NewHardSize
                | F::NewHardGeometryMode
                | F::NewHardPartitioning
                | F::NewHardDevice
                | F::NewHardFs
                | F::NewHardFsVariant
                | F::NewHardLabel
                | F::NewHardBootable
                | F::NewHardBootPri
                | F::NewHardReadOnly
                | F::NewHardSparse
                | F::NewHardCreate
                | F::NewGeomCylinders
                | F::NewGeomSurfaces
                | F::NewGeomSectors
                | F::NewGeomReserved
                | F::NewGeomVendor
                | F::NewGeomProduct
                | F::NewGeomRevision
                | F::NewGeomSave
                | F::NewGeomAuto
        )
    }

    /// Whether a field is one of the Serial section's TCP address boxes.
    /// They share the Create Image pages' free-text widget but hold machine
    /// configuration, so the drawing and click paths have to tell them apart.
    pub fn is_serial_addr(field: LauncherField) -> bool {
        #[cfg(feature = "midi")]
        {
            matches!(field, F::SerialConnect | F::SerialListen)
        }
        #[cfg(not(feature = "midi"))]
        {
            let _ = field;
            false
        }
    }

    /// Whether the value box for `field` is the one being typed into. Both
    /// the Create Image boxes and the serial address boxes are drawn by the
    /// same widget, so it asks this one question of either.
    pub fn typing_in_value_box(&self, field: LauncherField) -> bool {
        matches!(
            self.editing,
            Some(
                EditTarget::NewImageText(f)
                | EditTarget::Netplay(f)
                | EditTarget::SerialHost(f)
                | EditTarget::SerialPort(f)
            ) if f == field
        ) || field == F::RamPattern && self.editing == Some(EditTarget::RamPattern)
    }

    /// The filesystem a Create Image row is about: the floppy page's or the
    /// hard-drive page's, whichever row asked.
    pub fn workshop_fs_of(&self, field: LauncherField) -> Option<crate::diskimage::FileSystem> {
        match field {
            F::NewHardFs | F::NewHardFsVariant => self.workshop.hard_fs,
            _ => self.workshop.floppy_fs,
        }
    }

    /// Whether a family tick box is the chosen one.
    pub fn workshop_fs_family_set(&self, field: LauncherField, family: FsFamily) -> bool {
        FsFamily::of(self.workshop_fs_of(field)) == family
    }

    /// Whether a DOSType tick box shows as set.
    ///
    /// More than one can be: dircache and longname each *are* international,
    /// so choosing either lights the International box too. What cannot
    /// happen is dircache and longname together -- they are two values of
    /// one field.
    pub fn workshop_fs_variant_set(
        &self,
        field: LauncherField,
        variant: crate::diskimage::Variant,
    ) -> bool {
        use crate::diskimage::Variant as V;
        let Some(fs) = self.workshop_fs_of(field) else {
            return false;
        };
        match variant {
            V::Intl => fs.variant.is_intl(),
            V::DirCache => fs.variant.is_dircache(),
            V::LongName => fs.variant.is_longname(),
            V::Plain => false,
        }
    }

    /// Whether a DOSType tick box can be clicked.
    ///
    /// International is fixed on while a directory scheme is chosen: it
    /// comes with them, and there is no tag that has one without it. And
    /// each directory scheme turns the other away, because the tag holds
    /// one or neither.
    pub fn workshop_fs_variant_enabled(
        &self,
        field: LauncherField,
        variant: crate::diskimage::Variant,
    ) -> bool {
        use crate::diskimage::Variant as V;
        let Some(fs) = self.workshop_fs_of(field) else {
            return false;
        };
        match variant {
            V::Intl => !fs.variant.is_dircache() && !fs.variant.is_longname(),
            V::DirCache => !fs.variant.is_longname(),
            V::LongName => !fs.variant.is_dircache(),
            V::Plain => false,
        }
    }

    /// Choose a filesystem family, keeping the variant that was already
    /// picked: moving between OFS and FFS is a change of one bit, and
    /// silently dropping "international" with it would be a surprise.
    pub fn workshop_set_fs_family(&mut self, field: LauncherField, family: FsFamily) {
        let variant = self
            .workshop_fs_of(field)
            .map(|fs| fs.variant)
            .unwrap_or_default();
        let chosen = match family {
            FsFamily::Unformatted => None,
            FsFamily::Ofs => Some(crate::diskimage::FileSystem {
                ffs: false,
                variant,
            }),
            FsFamily::Ffs => Some(crate::diskimage::FileSystem { ffs: true, variant }),
        };
        match field {
            F::NewHardFs | F::NewHardFsVariant => self.workshop.hard_fs = chosen,
            _ => self.workshop.floppy_fs = chosen,
        }
    }

    /// Tick or clear one DOSType box, landing on whichever tag the boxes
    /// then describe.
    ///
    /// Clearing a directory scheme leaves International behind rather than
    /// going all the way back to plain: the box is still lit, so the state
    /// the user is looking at is the state they get.
    pub fn workshop_set_fs_variant(
        &mut self,
        field: LauncherField,
        variant: crate::diskimage::Variant,
    ) {
        use crate::diskimage::Variant as V;
        if !self.workshop_fs_variant_enabled(field, variant) {
            return;
        }
        let Some(mut fs) = self.workshop_fs_of(field) else {
            return;
        };
        let set = self.workshop_fs_variant_set(field, variant);
        fs.variant = match (variant, set) {
            (V::Intl, true) => V::Plain,
            (V::Intl, false) => V::Intl,
            (V::DirCache | V::LongName, true) => V::Intl,
            (V::DirCache, false) => V::DirCache,
            (V::LongName, false) => V::LongName,
            (V::Plain, _) => fs.variant,
        };
        match field {
            F::NewHardFs | F::NewHardFsVariant => self.workshop.hard_fs = Some(fs),
            _ => self.workshop.floppy_fs = Some(fs),
        }
    }

    /// A row's displayed value, from wherever that row's state lives.
    pub fn row_value(&self, field: LauncherField) -> String {
        if field.is_netplay() {
            return self.netplay.value(field);
        }
        if Self::is_workshop(field) {
            self.workshop_value(field)
        } else {
            self.setup.value_label(field)
        }
    }

    /// Whether a row's tick box is on, from wherever it lives.
    pub fn row_toggle(&self, field: LauncherField) -> bool {
        if field == F::NetplayEnabled {
            return self.netplay.enabled;
        }
        if Self::is_workshop(field) {
            self.workshop_toggle(field)
        } else {
            self.setup.toggle_value(field)
        }
    }

    /// Whether a row can be used at all, from wherever it lives.
    pub fn row_applies(&self, field: LauncherField) -> bool {
        if field.is_netplay() {
            return self.netplay.field_enabled(field);
        }
        if Self::is_workshop(field) {
            self.workshop_applies(field)
        } else {
            self.setup.applies(field)
        }
    }

    /// The value a Create Image row shows.
    pub fn workshop_value(&self, field: LauncherField) -> String {
        let w = &self.workshop;
        match field {
            F::NewFloppyDensity => w.density.label().to_string(),
            F::NewFloppyContainer => w.container.label().to_string(),
            F::NewFloppyLabel => w.floppy_label.clone(),
            F::NewHardSize => w.size.to_string(),
            F::NewHardBootPri => w.boot_pri.to_string(),
            F::NewGeomCylinders => w.custom_geometry.cylinders.to_string(),
            F::NewGeomSurfaces => w.custom_geometry.surfaces.to_string(),
            F::NewGeomSectors => w.custom_geometry.sectors.to_string(),
            F::NewGeomReserved => w.reserved.to_string(),
            F::NewGeomVendor => w.identity().vendor,
            F::NewGeomProduct => w.identity().product,
            F::NewGeomRevision => w.identity().revision,
            F::NewHardPartitioning => w.partitioning.label().to_string(),
            F::NewHardDevice => w.device.clone(),
            F::NewHardLabel => w.hard_label.clone(),
            _ => String::new(),
        }
    }

    /// Whether a Create Image toggle is on.
    pub fn workshop_toggle(&self, field: LauncherField) -> bool {
        match field {
            F::NewFloppyBootable => self.workshop.floppy_bootable,
            F::NewHardBootable => self.workshop.hard_bootable,
            F::NewHardReadOnly => self.workshop.read_only,
            F::NewHardSparse => self.workshop.sparse,
            _ => false,
        }
    }

    /// Whether a Create Image row can be used at all. Boot code needs a
    /// filesystem to load, so an unformatted disk has nothing to boot, and
    /// a volume name only means something once there is a volume.
    pub fn workshop_applies(&self, field: LauncherField) -> bool {
        let w = &self.workshop;
        match field {
            F::NewFloppyBootable | F::NewFloppyLabel => w.floppy_fs.is_some(),
            F::NewHardLabel => w.hard_fs.is_some(),
            // An unformatted volume has no DOS type, so there is nothing
            // for its identifiers to identify -- label and all.
            F::NewFloppyFsVariant => w.floppy_fs.is_some(),
            F::NewHardFsVariant => w.hard_fs.is_some(),
            // Without a partition table there is no partition entry to
            // carry a device name or a boot flag: the emulator names the
            // mount instead.
            F::NewHardDevice | F::NewHardBootable => {
                w.partitioning == crate::diskimage::Partitioning::Rdb
            }
            // Only a boot candidate has a rank among boot candidates.
            F::NewHardBootPri => {
                w.partitioning == crate::diskimage::Partitioning::Rdb && w.hard_bootable
            }
            _ => true,
        }
    }

    /// The button wording on a Create Image page's action row.
    pub fn workshop_action_label(&self, field: LauncherField) -> String {
        match field {
            // Both write a file, and the dialog that follows says which
            // kind: the page is already headed with that.
            F::NewFloppyCreate | F::NewHardCreate => "Save...".to_string(),
            F::NewGeomSave => "Apply".to_string(),
            F::NewGeomAuto => "Auto".to_string(),
            _ => String::new(),
        }
    }

    /// Step a Create Image picker.
    pub fn workshop_cycle(&mut self, field: LauncherField, forward: bool) {
        let w = &mut self.workshop;
        match field {
            F::NewFloppyDensity => {
                w.density = cycle_slice(&crate::diskimage::Density::ALL, w.density, forward)
            }
            F::NewFloppyContainer => {
                w.container = cycle_slice(&crate::diskimage::Container::ALL, w.container, forward)
            }
            // The geometry figures are the only stepped numbers here: the
            // size and the boot priority are typed, and have no arrows.
            //
            // Cylinder 0 goes to the Rigid Disk Block, so a drive needs a
            // second one before there is anything to partition.
            F::NewGeomCylinders => {
                w.custom_geometry.cylinders = step_u32(
                    w.custom_geometry.cylinders,
                    forward,
                    2,
                    workshop_ceiling(field),
                )
            }
            F::NewGeomSurfaces => {
                w.custom_geometry.surfaces = step_u32(
                    w.custom_geometry.surfaces,
                    forward,
                    1,
                    workshop_ceiling(field),
                )
            }
            F::NewGeomSectors => {
                w.custom_geometry.sectors = step_u32(
                    w.custom_geometry.sectors,
                    forward,
                    1,
                    workshop_ceiling(field),
                )
            }
            // The boot block is two blocks long, so two is the floor: with
            // fewer, the filesystem would be free to allocate over it.
            F::NewGeomReserved => {
                w.reserved = step_u32(
                    w.reserved,
                    forward,
                    crate::diskimage::RESERVED_BLOCKS,
                    workshop_ceiling(field),
                )
            }
            F::NewHardPartitioning => {
                w.partitioning = cycle_slice(
                    &crate::diskimage::Partitioning::ALL,
                    w.partitioning,
                    forward,
                )
            }
            _ => {}
        }
    }

    /// Flip a Create Image tick box.
    pub fn workshop_toggle_flip(&mut self, field: LauncherField) {
        let w = &mut self.workshop;
        match field {
            F::NewFloppyBootable => w.floppy_bootable = !w.floppy_bootable,
            F::NewHardBootable => w.hard_bootable = !w.hard_bootable,
            F::NewHardReadOnly => w.read_only = !w.read_only,
            F::NewHardSparse => w.sparse = !w.sparse,
            _ => {}
        }
    }

    /// Focus a Create Image text field, seeding the buffer with its value.
    pub fn begin_edit_new_image(&mut self, field: LauncherField) {
        if !self.workshop_applies(field) {
            return;
        }
        self.edit_buffer = self.workshop_value(field);
        self.editing = Some(EditTarget::NewImageText(field));
        self.edit_caret = Caret::end_of(&self.edit_buffer);
        self.status = None;
    }

    /// Focus half of a serial TCP address, seeding the buffer with the
    /// value that half holds. An unset half starts empty rather than from
    /// what it shows: the greyed prompts are placeholders, not values to
    /// be typed over.
    pub fn begin_edit_serial_addr(&mut self, field: LauncherField, port_half: bool) {
        if !Self::is_serial_addr(field) {
            return;
        }
        self.edit_buffer.clear();
        #[cfg(feature = "midi")]
        {
            let (host, port) = self.setup.serial_addr_parts(field);
            if port_half {
                if let Some(p) = port {
                    self.edit_buffer.push_str(&p.to_string());
                }
            } else if let Some(h) = host {
                self.edit_buffer.push_str(&h);
            }
        }
        self.editing = Some(if port_half {
            EditTarget::SerialPort(field)
        } else {
            EditTarget::SerialHost(field)
        });
        self.edit_caret = Caret::end_of(&self.edit_buffer);
        self.status = None;
    }

    /// Focus the fixed RAM word, using the canonical hexadecimal spelling the
    /// configuration writer will emit.
    pub fn begin_edit_ram_pattern(&mut self) {
        if !self.setup.applies(F::RamPattern) {
            return;
        }
        self.edit_buffer = self.setup.value_label(F::RamPattern);
        self.editing = Some(EditTarget::RamPattern);
        self.edit_caret = Caret::end_of(&self.edit_buffer);
        self.status = None;
    }

    pub fn new(setup: MachineSetup) -> Self {
        let mut setup = setup;
        // Read the host devices as the screen opens so the pickers show what is
        // connected now.
        setup.refresh_host_devices();
        // Likewise the WHDLoad support files: anything already sitting
        // where they go fills its own row rather than asking again.
        #[cfg(feature = "game-library")]
        setup.adopt_whdload_defaults();
        let mut state = Self {
            save_dialog: false,
            confirm_reset: false,
            #[cfg(feature = "game-library")]
            library: LibraryPage::default(),
            #[cfg(feature = "game-library")]
            openretro: None,
            #[cfg(feature = "game-library")]
            login: None,
            #[cfg(feature = "game-library")]
            meta: None,
            setup,
            workshop: ImageWorkshop::default(),
            netplay: NetplaySetup::default(),
            tab: LauncherTab::System,
            status: None,
            rom_notes: Default::default(),
            editing: None,
            edit_buffer: String::new(),
            edit_caret: Caret::default(),
        };
        // Name the ROMs the incoming configuration already carries.
        state.sync_rom_notes();
        state
    }

    /// Re-identify the ROM images when the chosen files change.
    ///
    /// Identification opens and checksums the image, so it must never happen
    /// on a redraw: the result is kept against the path it came from and this
    /// does nothing at all while that path stays put. Call it wherever a
    /// control may have changed the configuration (browsing to an image,
    /// clearing a row, resetting to defaults, loading a config file).
    pub fn sync_rom_notes(&mut self) {
        for (slot, field) in [(0, F::Rom), (1, F::ExtendedRom)] {
            let path = self.setup.path(field).map(Path::to_path_buf);
            if self.rom_notes[slot].seeded && self.rom_notes[slot].path == path {
                continue;
            }
            self.rom_notes[slot].text = path.as_deref().and_then(crate::config::rom_identification);
            // Only the Kickstart row draws identification lines; the
            // extended slot keeps just its raw text.
            if slot == 0 {
                self.rom_notes[slot].cells =
                    Self::rom_cells_for(path.as_deref(), self.rom_notes[slot].text.as_deref());
            }
            self.rom_notes[slot].path = path;
            self.rom_notes[slot].seeded = true;
        }
    }

    /// The Kickstart row's identification facts: a checksum-named image
    /// splits its label; an AROS image -- chosen or bundled -- reads its
    /// numbers off the file itself, so they follow the bundled ROM
    /// between releases.
    fn rom_cells_for(path: Option<&Path>, text: Option<&str>) -> (String, String, String) {
        let aros_cells = |file: &Path| {
            let (version, revision) = crate::romdb::rom_self_versions(file).unwrap_or_default();
            ("AROS".to_string(), version, revision)
        };
        match (path, text) {
            (Some(p), Some("bundled AROS")) => aros_cells(p),
            (_, Some(note)) => Self::split_identification(note),
            (Some(_), None) => (String::new(), String::new(), String::new()),
            // An empty slot boots the bundled AROS.
            (None, _) => match crate::romsearch::find_bundled_aros() {
                Some(bundle) => aros_cells(&bundle.main),
                None => (String::new(), String::new(), String::new()),
            },
        }
    }

    /// The identification lines' facts -- Name, Version and Revision --
    /// from the cache [`Self::sync_rom_notes`] keeps. Only the Kickstart
    /// row has them.
    pub fn rom_note_cells(&self, field: LauncherField) -> (String, String, String) {
        if field == F::Rom {
            self.rom_notes[0].cells.clone()
        } else {
            (String::new(), String::new(), String::new())
        }
    }

    /// Split a checksum label into its three facts. The labels read
    /// "Kickstart 3.1 (40.68) A1200": name words first, a marketing
    /// version, the ROM's own revision in parentheses, then the models
    /// -- which have no line of their own, and are dropped.
    fn split_identification(note: &str) -> (String, String, String) {
        let mut name_words: Vec<&str> = Vec::new();
        let mut version = String::new();
        let mut revision = String::new();
        for word in note.split_whitespace() {
            if version.is_empty()
                && word.chars().next().is_some_and(|c| c.is_ascii_digit())
                && word.contains('.')
            {
                version = word.to_string();
                continue;
            }
            if !version.is_empty() {
                // Only a numeric token is the revision: the parentheses
                // also carry variants ("Kickstart 1.0 A1000 (NTSC)"),
                // which are not one.
                if revision.is_empty() && word.starts_with('(') && word.ends_with(')') {
                    let inner = &word[1..word.len() - 1];
                    if !inner.is_empty() && inner.chars().all(|c| c.is_ascii_digit() || c == '.') {
                        revision = inner.to_string();
                    }
                }
                // Everything after the version that is not the revision
                // is the model list, which is not shown.
                continue;
            }
            name_words.push(word);
        }
        (name_words.join(" "), version, revision)
    }

    /// What the image on a ROM row was identified as: the raw checksum
    /// label the identification lines split from. `None` for an image no
    /// checksum names, and for every other field.
    pub fn rom_note(&self, field: LauncherField) -> Option<&str> {
        let slot = match field {
            F::Rom => 0,
            F::ExtendedRom => 1,
            _ => return None,
        };
        self.rom_notes[slot].text.as_deref()
    }

    /// Stand in for a recognised image, so the ROM tab can be drawn and
    /// tested without a real Kickstart on the host.
    #[cfg(test)]
    pub fn set_rom_note_for_test(&mut self, field: LauncherField, text: &str) {
        let slot = match field {
            F::ExtendedRom => 1,
            _ => 0,
        };
        self.rom_notes[slot].path = self.setup.path(field).map(Path::to_path_buf);
        self.rom_notes[slot].text = Some(text.to_string());
        self.rom_notes[slot].cells = Self::split_identification(text);
        self.rom_notes[slot].seeded = true;
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
        self.edit_caret = Caret::end_of(&self.edit_buffer);
        self.status = None;
    }

    /// Focus a drive's volume-name field, seeding the buffer with its value.
    pub fn begin_edit_drive_name(&mut self, field: LauncherField) {
        self.edit_buffer = self.setup.drive_name(field).unwrap_or_default().to_string();
        self.editing = Some(EditTarget::DriveName(field));
        self.edit_caret = Caret::end_of(&self.edit_buffer);
        self.status = None;
    }

    /// Focus a drive's boot-priority field for typing, seeding the buffer with
    /// the current number (a blank commit returns it to the unset default).
    pub fn begin_edit_drive_bootpri(&mut self, field: LauncherField) {
        // Do not offer typing on a greyed row (no image, a CD image, or a
        // drive whose Bootable box is cleared).
        if !self.setup.boot_field_applies(field) || self.setup.drive_boot_off(field) {
            return;
        }
        self.edit_buffer = match self.setup.drive_bootpri(field) {
            Some(n) => n.to_string(),
            None => String::new(),
        };
        self.editing = Some(EditTarget::DriveBootpri(field));
        self.edit_caret = Caret::end_of(&self.edit_buffer);
        self.status = None;
    }

    pub fn edit_push(&mut self, c: char) {
        let Some(target) = self.editing else { return };
        if let EditTarget::Netplay(field) = target {
            let limit = if field == F::NetplayCode && self.netplay.internet {
                4096
            } else if field == F::NetplayRelay {
                512
            } else if field == F::NetplayCode {
                32
            } else {
                64
            };
            if c.is_ascii() && !c.is_control() && self.edit_buffer.len() < limit {
                self.edit_caret.insert(&mut self.edit_buffer, c);
            }
            return;
        }
        // The size is a whole number of MB or GB: digits only, and no more
        // than the box accepts.
        if let EditTarget::NewImageText(field) = target {
            if let Some(limit) = workshop_digit_limit(field) {
                // A boot priority is the one signed figure here.
                let minus_ok = field == F::NewHardBootPri
                    && c == '-'
                    && self.edit_caret.at() == 0
                    && !self.edit_buffer.starts_with('-');
                if (!c.is_ascii_digit() && !minus_ok) || self.edit_buffer.len() >= limit {
                    return;
                }
                self.edit_caret.insert(&mut self.edit_buffer, c);
                return;
            }
            if let Some(limit) = workshop_text_limit(field) {
                // A drive identity is read back by tools that expect the
                // plain printable ASCII a SCSI INQUIRY carries, so nothing
                // else gets in -- and never more than the field holds.
                if !c.is_ascii_graphic() && c != ' ' {
                    return;
                }
                if self.edit_buffer.chars().count() >= limit {
                    return;
                }
                self.edit_caret.insert(&mut self.edit_buffer, c);
                return;
            }
        }
        // A host is a run of printable characters with no spaces in it: a
        // name or a numeric literal. Colons are in there for a bare IPv6
        // literal, so nothing narrower than "graphic" would do.
        if let EditTarget::SerialHost(_) = target {
            if !c.is_ascii_graphic() || self.edit_buffer.chars().count() >= SERIAL_HOST_MAX {
                return;
            }
        }
        // A port is digits, and no more of them than 65535 is wide.
        if let EditTarget::SerialPort(_) = target {
            if !c.is_ascii_digit() || self.edit_buffer.chars().count() >= SERIAL_PORT_MAX {
                return;
            }
        }
        // A fixed fill is a 16-bit word. The parser accepts decimal too, so
        // admit decimal digits and the hexadecimal prefix/digits while keeping
        // the buffer no wider than `0xFFFF`.
        if target == EditTarget::RamPattern
            && (self.edit_buffer.chars().count() >= 6
                || !(c.is_ascii_hexdigit() || matches!(c, 'x' | 'X')))
        {
            return;
        }
        // A boot priority is a signed integer: digits, and a leading minus.
        if let EditTarget::DriveBootpri(_) = target {
            let minus_ok =
                c == '-' && self.edit_caret.at() == 0 && !self.edit_buffer.starts_with('-');
            if !(c.is_ascii_digit() || minus_ok) {
                return;
            }
        }
        self.edit_caret.insert(&mut self.edit_buffer, c);
    }

    pub fn edit_backspace(&mut self) {
        if self.editing.is_some() {
            self.edit_caret.backspace(&mut self.edit_buffer);
        }
    }

    /// Delete forwards, from the character the caret is on.
    pub fn edit_delete(&mut self) {
        if self.editing.is_some() {
            self.edit_caret.delete(&mut self.edit_buffer);
        }
    }

    /// Step the caret through the box being typed in.
    pub fn edit_caret_move(&mut self, to: CaretMove) {
        if self.editing.is_none() {
            return;
        }
        match to {
            CaretMove::Left => self.edit_caret.left(),
            CaretMove::Right => self.edit_caret.right(&self.edit_buffer),
            CaretMove::Home => self.edit_caret.home(),
            CaretMove::End => self.edit_caret.end(&self.edit_buffer),
        }
    }

    /// Where the caret is in the box being typed in, for drawing it.
    pub fn edit_caret(&self) -> Caret {
        self.edit_caret
    }

    /// Commit the edit buffer to the focused field. A drive name that would
    /// not survive the config validator keeps the field focused, so the name
    /// can be fixed instead of failing later at save.
    pub fn edit_commit(&mut self) {
        let Some(target) = self.editing else { return };
        match target {
            EditTarget::Netplay(field) => {
                self.commit_netplay_edit(field);
                return;
            }
            EditTarget::DriveName(_) => {
                let name = self.edit_buffer.trim();
                let invalid = (!name.is_empty())
                    .then(|| crate::filesys::volume_name_error(name))
                    .flatten();
                if let Some(err) = invalid {
                    self.status = Some(StatusMessage::err(err));
                    return;
                }
            }
            EditTarget::NewImageText(field) if workshop_digit_limit(field).is_some() => {
                // Every one of these is a number with a floor, so an empty
                // or unreadable box returns to the floor rather than
                // refusing: there is no wrong value to complain about.
                let typed = self.edit_buffer.trim();
                let w = &mut self.workshop;
                match field {
                    F::NewHardSize => {
                        w.size = typed
                            .parse::<u32>()
                            .unwrap_or(0)
                            .clamp(1, NEW_HARD_SIZE_MAX)
                    }
                    F::NewHardBootPri => w.boot_pri = typed.parse::<i8>().unwrap_or(0),
                    F::NewGeomCylinders => {
                        w.custom_geometry.cylinders = typed
                            .parse::<u32>()
                            .unwrap_or(0)
                            .clamp(2, workshop_ceiling(field))
                    }
                    F::NewGeomSurfaces => {
                        w.custom_geometry.surfaces = typed
                            .parse::<u32>()
                            .unwrap_or(0)
                            .clamp(1, workshop_ceiling(field))
                    }
                    F::NewGeomSectors => {
                        w.custom_geometry.sectors = typed
                            .parse::<u32>()
                            .unwrap_or(0)
                            .clamp(1, workshop_ceiling(field))
                    }
                    F::NewGeomReserved => {
                        w.reserved = typed
                            .parse::<u32>()
                            .unwrap_or(0)
                            .clamp(crate::diskimage::RESERVED_BLOCKS, workshop_ceiling(field))
                    }
                    _ => {}
                }
                self.editing = None;
                self.edit_buffer.clear();
                return;
            }
            EditTarget::NewImageText(field) if workshop_text_limit(field).is_some() => {
                // Not an AmigaDOS name: an identity field is a run of
                // printable bytes a tool prints back, so anything typed
                // into it is already valid by the time it lands. Emptying
                // one is a choice too -- the field goes to spaces.
                let text = self.edit_buffer.trim().to_string();
                self.workshop.set_identity_field(field, text);
                self.editing = None;
                self.edit_buffer.clear();
                return;
            }
            EditTarget::NewImageText(field) => {
                let text = self.edit_buffer.trim();
                // A volume name is an AmigaDOS name and has to survive
                // being one; a device name is what the mount is called.
                if let Some(err) = crate::filesys::volume_name_error(text) {
                    self.status = Some(StatusMessage::err(err));
                    return;
                }
                let text = text.to_string();
                match field {
                    F::NewFloppyLabel => self.workshop.floppy_label = text,
                    F::NewHardLabel => self.workshop.hard_label = text,
                    F::NewHardDevice => self.workshop.device = text,
                    _ => {}
                }
                self.editing = None;
                self.edit_buffer.clear();
                return;
            }
            EditTarget::DriveBootpri(field) => {
                // A bad number keeps the field focused so it can be fixed, the
                // same as a rejected drive name. Typing the -128 sentinel clears
                // the Bootable box rather than storing an out-of-range priority.
                match parse_drive_bootpri(&self.edit_buffer) {
                    Ok(Some(BOOT_PRI_NEVER)) => self.setup.set_drive_boot_off(field, true),
                    Ok(value) => {
                        self.setup.set_drive_boot_off(field, false);
                        self.setup.set_drive_bootpri(field, value);
                    }
                    Err(err) => {
                        self.status = Some(StatusMessage::err(err.to_string()));
                        return;
                    }
                }
                self.editing = None;
                self.edit_buffer.clear();
                return;
            }
            EditTarget::SerialHost(field) => {
                // An emptied box means "unset": the half reverts to its
                // greyed default. Brackets around an IPv6 literal are the
                // stored spelling's business, not the typist's.
                let typed = self.edit_buffer.trim().to_string();
                let typed = typed.as_str();
                // The old single box took host:port, and fingers remember.
                // A lone trailing :digits on a colon-free head is that
                // habit, not an IPv6 literal, so the halves go where they
                // belong -- through the port's own checks.
                if let Some((head, tail)) = typed.rsplit_once(':') {
                    if !head.is_empty()
                        && !head.contains(':')
                        && !tail.is_empty()
                        && tail.chars().all(|c| c.is_ascii_digit())
                    {
                        let Some(port) = self.checked_port(field, tail) else {
                            return;
                        };
                        #[cfg(feature = "midi")]
                        self.setup.set_serial_addr_part(
                            field,
                            Some(Some(head.to_string())),
                            Some(Some(port)),
                        );
                        #[cfg(not(feature = "midi"))]
                        let _ = (field, port);
                        self.editing = None;
                        self.edit_buffer.clear();
                        return;
                    }
                }
                let host = (!typed.is_empty()).then(|| {
                    typed
                        .strip_prefix('[')
                        .and_then(|h| h.strip_suffix(']'))
                        .unwrap_or(typed)
                        .to_string()
                });
                #[cfg(feature = "midi")]
                self.setup.set_serial_addr_part(field, Some(host), None);
                #[cfg(not(feature = "midi"))]
                let _ = (field, host);
                self.editing = None;
                self.edit_buffer.clear();
                return;
            }
            EditTarget::SerialPort(field) => {
                // Emptied, the port reverts to its default. A typed one has
                // to be a port this run can actually take, or it keeps the
                // focus where the mistake is -- the mode is bound to fail at
                // Run otherwise.
                let typed = self.edit_buffer.trim().to_string();
                let typed = typed.as_str();
                let port = if typed.is_empty() {
                    None
                } else {
                    let Some(p) = self.checked_port(field, typed) else {
                        return;
                    };
                    Some(p)
                };
                #[cfg(feature = "midi")]
                self.setup.set_serial_addr_part(field, None, Some(port));
                #[cfg(not(feature = "midi"))]
                let _ = (field, port);
                self.editing = None;
                self.edit_buffer.clear();
                return;
            }
            EditTarget::RamPattern => {
                match crate::config::parse_ram_pattern(&self.edit_buffer) {
                    Ok(word) => self.setup.set_ram_pattern(word),
                    Err(err) => {
                        self.status = Some(StatusMessage::err(err.to_string()));
                        return;
                    }
                }
                self.editing = None;
                self.edit_buffer.clear();
                return;
            }
            EditTarget::BoardOption { .. } => {}
        }
        self.editing = None;
        let value = std::mem::take(&mut self.edit_buffer);
        match target {
            EditTarget::BoardOption { board, opt } => {
                self.setup.zorro_option_set(board, opt, value)
            }
            EditTarget::DriveName(field) => self.setup.set_drive_name(field, value),
            // These commit above and return, so nothing is left to do here.
            EditTarget::Netplay(_)
            | EditTarget::DriveBootpri(_)
            | EditTarget::NewImageText(_)
            | EditTarget::SerialHost(_)
            | EditTarget::SerialPort(_)
            | EditTarget::RamPattern => {}
        }
    }

    /// A typed port, checked: in range, and -- for the listen address,
    /// the only one this process binds -- allowed to this process. A
    /// dial-out port needs no privilege on any host, telnet's 23 included.
    /// `None` sets the refusal message and keeps the focus.
    fn checked_port(&mut self, field: LauncherField, typed: &str) -> Option<u16> {
        match typed.parse::<u32>() {
            Ok(p @ 1..=65535) => {
                // A warning, not a refusal: the effective uid is only a
                // hint (Linux grants low ports via CAP_NET_BIND_SERVICE
                // and ip_unprivileged_port_start too), so the bind at Run
                // stays the authority and says so itself if it disagrees.
                #[cfg(all(unix, feature = "midi"))]
                if p < 1025 && field == LauncherField::SerialListen && !can_bind_low_ports() {
                    self.status = Some(StatusMessage::err(format!(
                        "port {p} usually needs root on macOS/Linux; the run will say if it cannot bind"
                    )));
                }
                #[cfg(not(all(unix, feature = "midi")))]
                let _ = field;
                Some(p as u16)
            }
            _ => {
                self.status = Some(StatusMessage::err(format!(
                    "\"{typed}\" is not a port number (1-65535)"
                )));
                None
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
    matches!(
        cpu,
        CpuModel::M68020 | CpuModel::M68030 | CpuModel::M68040 | CpuModel::M68060
    )
}

/// Whether `field` appears anywhere with the given row kind. Used to classify a
/// field (toggle vs path) without threading the tab through every call, called
/// per drawn row, so it scans the static row tables directly rather than
/// composing tabs (which would allocate every frame). The composed tabs only
/// add `SectionHeader`/`BootpriHeader` rows, which carry no real field, so the
/// raw tables cover every classifiable field.
fn rows_contains_kind(field: LauncherField, kind: RowKind) -> bool {
    #[cfg(all(feature = "midi", feature = "mt32", feature = "coppersynth"))]
    let serial: &[&[Row]] = &[
        &SERIAL_ROWS_MIDI,
        &SERIAL_ROWS_MT32,
        &SERIAL_ROWS_CSYNTH,
        &SERIAL_ROWS_TCP_CONNECT,
        &SERIAL_ROWS_TCP_LISTEN,
        &SERIAL_ROWS_MODEM,
    ];
    #[cfg(all(feature = "midi", feature = "mt32", not(feature = "coppersynth")))]
    let serial: &[&[Row]] = &[
        &SERIAL_ROWS_MIDI,
        &SERIAL_ROWS_MT32,
        &SERIAL_ROWS_TCP_CONNECT,
        &SERIAL_ROWS_TCP_LISTEN,
        &SERIAL_ROWS_MODEM,
    ];
    #[cfg(all(feature = "midi", not(feature = "mt32"), feature = "coppersynth"))]
    let serial: &[&[Row]] = &[
        &SERIAL_ROWS_MIDI,
        &SERIAL_ROWS_CSYNTH,
        &SERIAL_ROWS_TCP_CONNECT,
        &SERIAL_ROWS_TCP_LISTEN,
        &SERIAL_ROWS_MODEM,
    ];
    #[cfg(all(feature = "midi", not(feature = "mt32"), not(feature = "coppersynth")))]
    let serial: &[&[Row]] = &[
        &SERIAL_ROWS_MIDI,
        &SERIAL_ROWS_TCP_CONNECT,
        &SERIAL_ROWS_TCP_LISTEN,
        &SERIAL_ROWS_MODEM,
    ];
    #[cfg(not(feature = "midi"))]
    let serial: &[&[Row]] = &[];
    let sources: &[&[Row]] = &[
        &SYSTEM_ROWS,
        &CPU_ROWS,
        &MEMORY_ROWS,
        &ROM_ROWS,
        &FLOPPY_ROWS,
        &STORAGE_ROWS,
        &HOSTFS_ROWS,
        &WHDLOAD_ROWS,
        &CD_ROWS,
        &LIDE_ROWS,
        &COPPERHF_ROWS,
        &INPUT_ROWS,
        &VIDEO_ROWS,
        &AUDIO_ROWS,
        &EMULATION_ROWS,
        &PARALLEL_ROWS_PRINTER,
        &PARALLEL_ROWS_SAMPLER,
    ];
    sources
        .iter()
        .chain(serial.iter())
        .flat_map(|table| table.iter())
        .any(|r| r.field == field && r.kind == kind)
}

fn path_string(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

/// Build a `[ide]`/`[scsi]` drive entry from an editable path + optional
/// volume-name override. A blank name emits the bare path string so saved
/// configs stay minimal.
fn drive_raw(
    path: Option<&Path>,
    name: Option<&str>,
    bootpri: Option<i8>,
    filesystem: crate::diskimage::FileSystem,
) -> Option<RawDrive> {
    path.map(|p| RawDrive {
        path: path_string(p),
        name: name
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string),
        bootpri,
        // OFS is only meaningful (and only valid, config-side) on a
        // directory mount; FFS is the default, so it too stays unwritten,
        // keeping a saved config minimal exactly like `name` above.
        filesystem: (p.is_dir() && !filesystem.ffs).then(|| "ofs".to_string()),
    })
}

/// A `[scsi]` unit entry by SCSI ID. `RawScsi` names its units as separate
/// fields (that is the TOML shape), so indexed access needs this shim.
fn raw_scsi_unit(scsi: &crate::config::RawScsi, unit: usize) -> Option<&RawDrive> {
    match unit {
        0 => scsi.unit0.as_ref(),
        1 => scsi.unit1.as_ref(),
        2 => scsi.unit2.as_ref(),
        3 => scsi.unit3.as_ref(),
        4 => scsi.unit4.as_ref(),
        5 => scsi.unit5.as_ref(),
        _ => scsi.unit6.as_ref(),
    }
}

/// A `[copperhf]` unit entry by unit number, paralleling `raw_scsi_unit`.
fn raw_copperhf_unit(copperhf: &crate::config::RawCopperhf, unit: usize) -> Option<&RawDrive> {
    match unit {
        0 => copperhf.unit0.as_ref(),
        1 => copperhf.unit1.as_ref(),
        2 => copperhf.unit2.as_ref(),
        3 => copperhf.unit3.as_ref(),
        4 => copperhf.unit4.as_ref(),
        5 => copperhf.unit5.as_ref(),
        _ => copperhf.unit6.as_ref(),
    }
}

/// Boot priorities offered on the Host FS boot-pri stepper. -128 is the
/// "never boot" sentinel; the rest bracket the usual device priorities
/// (hard-disk partitions boot at 0, DF0: at 5).
const BOOTPRI_STEPS: [i8; 8] = [-128, -10, -5, 0, 5, 6, 10, 20];

/// Display form of a hard-disk boot priority in the Priority column: the number,
/// with unset shown as 0 (its effective value). A cleared Bootable box arrives
/// as the -128 "never" sentinel and displays as -128.
fn drive_bootpri_label(pri: Option<i8>) -> String {
    pri.unwrap_or(0).to_string()
}

/// Lowest priority the arrows/typing may reach while Bootable is ticked; -128 is
/// reserved for the cleared box, so an enabled priority stops at -127.
const BOOT_PRI_MIN_ENABLED: i8 = BOOT_PRI_NEVER + 1;

/// Nudge a hard-disk boot priority by one, clamped to -127..=127 (the -128
/// sentinel is the Bootable box, not a step). Unset steps off from 0.
fn step_drive_bootpri(current: Option<i8>, forward: bool) -> Option<i8> {
    let base = current.unwrap_or(0);
    let stepped = if forward {
        base.saturating_add(1)
    } else {
        base.saturating_sub(1)
    };
    Some(stepped.clamp(BOOT_PRI_MIN_ENABLED, i8::MAX))
}

/// The cascade default for the `rank`-th present hard-disk drive when the config
/// set no priority: the first is 0 (a strong boot candidate, just under DF0's
/// 5), and every later drive drops below all four floppies -- -35, -40, -45...
/// `None` (rank 0) leaves the key unwritten; the rest are explicit.
fn hdd_boot_cascade(rank: usize) -> Option<i8> {
    (rank > 0).then(|| (-30 - 5 * rank as i32).max(i8::MIN as i32) as i8)
}

/// Split a config `bootpri` into the launcher's two slots: the -128 "never"
/// sentinel clears the Bootable box (priority unset), any other value is the
/// priority, and a missing key is unset.
fn boot_priority_of(raw: Option<i8>) -> Option<i8> {
    raw.filter(|&p| p != BOOT_PRI_NEVER)
}

fn boot_is_off(raw: Option<i8>) -> bool {
    raw == Some(BOOT_PRI_NEVER)
}

/// SCSI unit index behind a `ScsiUnitN`/`ScsiUnitNBoot` field.
fn scsi_boot_index(field: LauncherField) -> Option<usize> {
    use LauncherField as F;
    Some(match field {
        F::ScsiUnit0 | F::ScsiUnit0Boot => 0,
        F::ScsiUnit1 | F::ScsiUnit1Boot => 1,
        F::ScsiUnit2 | F::ScsiUnit2Boot => 2,
        F::ScsiUnit3 | F::ScsiUnit3Boot => 3,
        F::ScsiUnit4 | F::ScsiUnit4Boot => 4,
        F::ScsiUnit5 | F::ScsiUnit5Boot => 5,
        F::ScsiUnit6 | F::ScsiUnit6Boot => 6,
        _ => return None,
    })
}

/// Lide drive index behind a `LideDriveN`/`LideDriveNBoot` field.
fn lide_drive_index(field: LauncherField) -> Option<usize> {
    use LauncherField as F;
    Some(match field {
        F::LideDrive0 | F::LideDrive0Boot => 0,
        F::LideDrive1 | F::LideDrive1Boot => 1,
        F::LideDrive2 | F::LideDrive2Boot => 2,
        F::LideDrive3 | F::LideDrive3Boot => 3,
        _ => return None,
    })
}

/// copperhf unit index behind a `CopperhfUnitN`/`CopperhfUnitNBoot` field.
fn copperhf_unit_index(field: LauncherField) -> Option<usize> {
    use LauncherField as F;
    Some(match field {
        F::CopperhfUnit0 | F::CopperhfUnit0Boot => 0,
        F::CopperhfUnit1 | F::CopperhfUnit1Boot => 1,
        F::CopperhfUnit2 | F::CopperhfUnit2Boot => 2,
        F::CopperhfUnit3 | F::CopperhfUnit3Boot => 3,
        F::CopperhfUnit4 | F::CopperhfUnit4Boot => 4,
        F::CopperhfUnit5 | F::CopperhfUnit5Boot => 5,
        F::CopperhfUnit6 | F::CopperhfUnit6Boot => 6,
        _ => return None,
    })
}

/// The boot-priority field for a hard-disk drive field (inverse of
/// [`MachineSetup::boot_field_drive`]).
fn drive_boot_field(drive: LauncherField) -> Option<LauncherField> {
    use LauncherField as F;
    Some(match drive {
        F::IdeMaster => F::IdeMasterBoot,
        F::IdeSlave => F::IdeSlaveBoot,
        F::ScsiUnit0 => F::ScsiUnit0Boot,
        F::ScsiUnit1 => F::ScsiUnit1Boot,
        F::ScsiUnit2 => F::ScsiUnit2Boot,
        F::ScsiUnit3 => F::ScsiUnit3Boot,
        F::ScsiUnit4 => F::ScsiUnit4Boot,
        F::ScsiUnit5 => F::ScsiUnit5Boot,
        F::ScsiUnit6 => F::ScsiUnit6Boot,
        F::LideDrive0 => F::LideDrive0Boot,
        F::LideDrive1 => F::LideDrive1Boot,
        F::LideDrive2 => F::LideDrive2Boot,
        F::LideDrive3 => F::LideDrive3Boot,
        F::CopperhfUnit0 => F::CopperhfUnit0Boot,
        F::CopperhfUnit1 => F::CopperhfUnit1Boot,
        F::CopperhfUnit2 => F::CopperhfUnit2Boot,
        F::CopperhfUnit3 => F::CopperhfUnit3Boot,
        F::CopperhfUnit4 => F::CopperhfUnit4Boot,
        F::CopperhfUnit5 => F::CopperhfUnit5Boot,
        F::CopperhfUnit6 => F::CopperhfUnit6Boot,
        _ => return None,
    })
}

/// Parse typed Boot Priority input: empty returns to the unset default (`None`),
/// any integer in -128..=127 becomes that value (-128 clears Bootable at the
/// call site), anything else is rejected.
fn parse_drive_bootpri(text: &str) -> std::result::Result<Option<i8>, &'static str> {
    let text = text.trim();
    if text.is_empty() {
        return Ok(None);
    }
    text.parse::<i8>()
        .map(Some)
        .map_err(|_| "boot priority must be -128..127")
}

fn cycle_bootpri(current: i8, forward: bool) -> i8 {
    let idx = BOOTPRI_STEPS
        .iter()
        .position(|&p| p == current)
        .unwrap_or_else(|| {
            // An off-list value (hand-edited config): snap to the nearest.
            BOOTPRI_STEPS
                .iter()
                .enumerate()
                .min_by_key(|(_, &p)| (i32::from(p) - i32::from(current)).abs())
                .map(|(i, _)| i)
                .unwrap_or(0)
        });
    let n = BOOTPRI_STEPS.len();
    let next = if forward {
        (idx + 1) % n
    } else {
        (idx + n - 1) % n
    };
    BOOTPRI_STEPS[next]
}

/// The tail a MIDI picker's list carries when the built-in module belongs
/// on it: the module is not a host endpoint, so it is added rather than
/// enumerated. Nothing to add on a build without it.
#[cfg(feature = "midi")]
fn mt32_endpoint(wanted: bool) -> Option<String> {
    (wanted && cfg!(feature = "mt32")).then(|| crate::config::MIDI_OUT_MT32.to_string())
}

#[cfg(feature = "midi")]
fn csynth_endpoint(wanted: bool) -> Option<String> {
    (wanted && cfg!(feature = "coppersynth")).then(|| crate::config::MIDI_OUT_CSYNTH.to_string())
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

/// Cycle a network board's fitted/backend state (`None` = not fitted)
/// through the backends this build can actually bring up: NAT only where
/// available, a bridge choice only when an adapter exists to name (keeping
/// the current adapter if the board is already bridged). Shared by the
/// A2065 and HostSocket pickers in the Ethernet section.
fn cycle_net_board(
    net: &mut Option<NetConfig>,
    bridge_interfaces: &[(String, String)],
    forward: bool,
) {
    let mut choices = vec![None, Some(NetConfig::None), Some(NetConfig::Loopback)];
    if crate::net::NAT_AVAILABLE {
        choices.push(Some(NetConfig::Nat));
    }
    if crate::net::BRIDGE_AVAILABLE {
        let interface = match net.as_ref() {
            Some(NetConfig::Bridge { interface }) => Some(interface.clone()),
            _ => bridge_interfaces.first().map(|(name, _)| name.clone()),
        };
        if let Some(interface) = interface {
            choices.push(Some(NetConfig::Bridge { interface }));
        }
    }
    let index = choices.iter().position(|item| item == net).unwrap_or(0);
    let next = if forward {
        (index + 1) % choices.len()
    } else {
        (index + choices.len() - 1) % choices.len()
    };
    *net = choices[next].clone();
}

/// `cycle_net_board`'s HostSocket-only counterpart: same choices, plus one
/// more this board alone supports -- `Host`, real host OS sockets via
/// direct `bsdsocket.library` passthrough (`crate::hostsocket`'s own doc
/// comment). Not a `NetConfig` backend at all (the A2065 is a real
/// Ethernet card; it has no such passthrough to offer), so it can't be a
/// `cycle_net_board` choice -- represented here by `host_mode`, a flag
/// alongside `net` rather than a `NetConfig` variant of its own. `net` is
/// irrelevant once `host_mode` is set (`Config::from_raw` always resolves
/// `net = "host"` to a `Loopback` smoltcp backend underneath regardless of
/// what the launcher's own `net` field holds -- see `to_raw`'s own
/// `hostsocket_host_mode` branch, which ignores it entirely when saving),
/// so `Host` is simply appended as one final `(None, true)` step before
/// wrapping back to plain `None`.
fn cycle_hostsocket_board(
    net: &mut Option<NetConfig>,
    host_mode: &mut bool,
    bridge_interfaces: &[(String, String)],
    forward: bool,
) {
    let mut choices: Vec<(Option<NetConfig>, bool)> = vec![
        (None, false),
        (Some(NetConfig::None), false),
        (Some(NetConfig::Loopback), false),
    ];
    if crate::net::NAT_AVAILABLE {
        choices.push((Some(NetConfig::Nat), false));
    }
    if crate::net::BRIDGE_AVAILABLE {
        let interface = match net.as_ref() {
            Some(NetConfig::Bridge { interface }) => Some(interface.clone()),
            _ => bridge_interfaces.first().map(|(name, _)| name.clone()),
        };
        if let Some(interface) = interface {
            choices.push((Some(NetConfig::Bridge { interface }), false));
        }
    }
    choices.push((None, true));
    let current = if *host_mode {
        (None, true)
    } else {
        (net.clone(), false)
    };
    let index = choices
        .iter()
        .position(|item| item == &current)
        .unwrap_or(0);
    let next = if forward {
        (index + 1) % choices.len()
    } else {
        (index + choices.len() - 1) % choices.len()
    };
    let (chosen_net, chosen_host_mode) = choices[next].clone();
    *net = chosen_net;
    *host_mode = chosen_host_mode;
}

/// Cycle a bridged network board's host adapter through the visible ones;
/// inert unless the board's backend is currently a bridge.
fn cycle_bridge_interface(
    net: &mut Option<NetConfig>,
    bridge_interfaces: &[(String, String)],
    forward: bool,
) {
    if let Some(NetConfig::Bridge { interface }) = net.as_mut() {
        if !bridge_interfaces.is_empty() {
            let index = bridge_interfaces
                .iter()
                .position(|(name, _)| name == interface)
                .unwrap_or(0);
            let next = if forward {
                (index + 1) % bridge_interfaces.len()
            } else {
                (index + bridge_interfaces.len() - 1) % bridge_interfaces.len()
            };
            *interface = bridge_interfaces[next].0.clone();
        }
    }
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

/// A drive count represents the contiguous set DF0 through the highest
/// connected bay, not the number of true entries. Configs without an explicit
/// count may connect only a higher bay when media names it.
fn connected_floppy_bays(connected: &[bool; 4]) -> u8 {
    connected
        .iter()
        .rposition(|&connected| connected)
        .map(|idx| idx as u8 + 1)
        .unwrap_or(0)
}

/// A seconds label that keeps a fractional configured value visible
/// ("12.5s") while the whole-second presets stay terse ("10s"). The
/// config accepts arbitrary positive seconds, so the panel must not
/// round a loaded value into a different-looking timestamp.
fn format_secs(secs: f64) -> String {
    if secs.fract().abs() < 1e-6 {
        format!("{secs:.0}s")
    } else {
        format!("{secs}s")
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
        MachineModel::A3000 => "A3000",
        MachineModel::A4000 => "A4000",
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
        MachineModel::A3000 => "A3000",
        MachineModel::A4000 => "A4000",
        MachineModel::Cdtv => "CDTV",
        MachineModel::Cd32 => "CD32",
        MachineModel::A1000 => "A1000",
    }
}

fn rtg_card_name(card: RtgCard) -> &'static str {
    match card {
        RtgCard::None => "None",
        RtgCard::Picasso2 => "Picasso II",
        RtgCard::Picasso2Plus => "Picasso II+",
        RtgCard::Z3660 => "Z3660",
        RtgCard::GraffityZ2 => "Graffity Z2",
        RtgCard::GraffityZ3 => "Graffity Z3",
    }
}

fn rtg_card_value(card: RtgCard) -> &'static str {
    match card {
        RtgCard::None => "none",
        RtgCard::Picasso2 => "picasso2",
        RtgCard::Picasso2Plus => "picasso2plus",
        RtgCard::Z3660 => "z3660",
        RtgCard::GraffityZ2 => "graffityz2",
        RtgCard::GraffityZ3 => "graffityz3",
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
        CpuModel::M68010 => "68010",
        CpuModel::M68EC020 => "68EC020",
        CpuModel::M68020 => "68020",
        CpuModel::M68030 => "68030",
        CpuModel::M68040 => "68040",
        CpuModel::M68060 => "68060",
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

pub(crate) fn pixel_aspect_name(aspect: PixelAspect) -> &'static str {
    match aspect {
        PixelAspect::Tv => "tv",
        PixelAspect::Square => "square",
    }
}

pub(crate) fn display_scaling_name(scaling: DisplayScaling) -> &'static str {
    match scaling {
        DisplayScaling::Smooth => "smooth",
        DisplayScaling::Integer => "integer",
    }
}

/// The `[display] shader` value for a mode: the canonical preset name (not
/// the picker's "off" spelling, which only parses back), or a user shader's
/// path as written.
fn shader_name(shader: &ShaderMode) -> String {
    match shader {
        ShaderMode::None => "none".to_string(),
        ShaderMode::Scanlines => "scanlines".to_string(),
        ShaderMode::Mask => "mask".to_string(),
        ShaderMode::Crt => "crt".to_string(),
        ShaderMode::Custom(path) => path_string(path),
    }
}

/// The `[display] tint` value for a tint: the canonical config name (not
/// the picker's "off" spelling, which only parses back).
pub(crate) fn tint_name(tint: Tint) -> &'static str {
    match tint {
        Tint::None => "none",
        Tint::Bw => "bw",
        Tint::Green => "green",
        Tint::Amber => "amber",
        Tint::Sepia => "sepia",
    }
}

fn pacing_name(pacing: PacingBudget) -> &'static str {
    match pacing {
        PacingBudget::Cycles => "cycles",
        PacingBudget::Instructions => "instructions",
    }
}

#[cfg(test)]
mod tests;
