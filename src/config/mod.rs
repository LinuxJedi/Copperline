// SPDX-License-Identifier: GPL-3.0-or-later

//! Loadable configuration. The file format is TOML; see
//! `copperline.example.toml` (or the README) for the full schema.

use crate::bus::PortDevice;
use crate::chipset::agnus::{AgnusRevision, VideoStandard};
use crate::chipset::denise::DeniseRevision;
use crate::memory::{RamInit, DEFAULT_RANDOM_RAM_SEED};
use crate::zorro::{zorro_ii_size_code, zorro_iii_size_bits, BoardSpec, ZorroChain, ZorroVersion};
use anyhow::{anyhow, bail, Result};
use std::path::{Path, PathBuf};

mod about;
mod raw;
mod resolve;
#[cfg(test)]
mod tests;
mod validate;

pub use about::*;
pub use raw::*;
pub use resolve::*;
pub use validate::*;

/// Skip-serializing predicate for the raw config's nested `[section]` structs:
/// a section that still equals its default carries no user-set field, so it is
/// omitted from saved TOML entirely (keeping written files minimal, like the
/// hand-written `*.example.toml`). Referenced from `#[serde(skip_serializing_if
/// = "is_default")]` on each section field.
fn is_default<T: Default + PartialEq>(value: &T) -> bool {
    *value == T::default()
}

/// Sentinel `rom_path` meaning "the user named no ROM": boot the bundled
/// AROS open-source Kickstart replacement if it can be found, otherwise fail
/// with a message telling the user to supply a Kickstart. A real path (from
/// `rom = "..."` or the CLI argument) always replaces it.
pub const BUNDLED_AROS_ROM: &str = "<bundled-aros>";

/// Sentinel `fmv_rom_path` for the CD32 profile's freely redistributable FMV
/// cartridge ROM. `fmv_rom = "..."` replaces it; `fmv_rom = ""` explicitly
/// leaves the cartridge slot empty.
pub const BUNDLED_FMV_ROM: &str = "<bundled-fmv>";

/// The player build's persisted end-user settings, an ordinary
/// configuration fragment under the per-game config directory. Written by
/// the menu's write-through in player sessions and layered over the game
/// manifest's defaults by the player's startup.
pub const PLAYER_SETTINGS_FILE: &str = "settings.toml";

/// Sentinel `[scsi] rom` for an A4091 fitted without a named ROM: resolve to
/// the bundled A4091 ROM, or fail. A real `rom = "..."` replaces it.
pub const BUNDLED_A4091_ROM: &str = "<bundled-a4091>";

/// Sentinel `[cartridge] rom` for a freezer cartridge fitted without a
/// named image: resolve to the bundled HRTMon image, or fail. A real
/// `rom = "..."` replaces it.
pub const BUNDLED_HRTMON_ROM: &str = "<bundled-hrtmon>";

/// Sentinel `[scsi] rom` for an A2091/A590 fitted without a named ROM:
/// resolve to Copperline's open replacement ROM, or fail. Explicit merged
/// and split-EPROM paths continue to take precedence.
pub const BUNDLED_A2091_ROM: &str = "<bundled-a2091>";

/// Sentinel `[lide] rom` for a board fitted (an explicit `board`, or a drive
/// image) without a named ROM: resolve to the bundled ROM for that board's
/// personality (`lide.rom` for RIPPLE/RIDE, `lide-atbus.rom` for AT-Bus
/// 2008 -- the two are not interchangeable), or fail. A real `rom = "..."`
/// replaces it; `rom = ""` opts out and keeps the board in hardware-only
/// mode.
pub const BUNDLED_LIDE_ROM: &str = "<bundled-lide>";

/// Sentinel `[lide] rom_bank2` alongside [`BUNDLED_LIDE_ROM`]: resolve to the
/// bundled `cdfs.rom` (RIPPLE/RIDE only -- AT-Bus 2008 has no flash
/// banking). A real `rom_bank2 = "..."` replaces it; `rom_bank2 = ""` opts
/// out.
pub const BUNDLED_LIDE_CDFS_ROM: &str = "<bundled-lide-cdfs>";

/// A WASM plugin Zorro board resolved from config: its autoconfig identity
/// (`spec`, with a placeholder device slot reassigned at build time), the
/// `.wasm` module path, and the plugin manifest (name + capabilities).
#[derive(Debug, Clone)]
pub struct WasmBoardConfig {
    pub spec: BoardSpec,
    pub wasm_path: PathBuf,
    pub manifest: crate::wasm_manifest::WasmManifest,
}

#[derive(Debug, Clone)]
pub struct Config {
    pub rom_path: PathBuf,
    /// Installed only by netplay preparation, never by a configuration file.
    /// All hard-drive opens then use private rollback-aware session storage.
    #[doc(hidden)]
    pub netplay_storage: bool,
    #[doc(hidden)]
    pub netplay_read_only: Vec<PathBuf>,
    pub cpu: CpuModel,
    pub fpu: bool,
    /// CPU clock in MHz. Defaults to the model's stock speed
    /// ([`CpuModel::default_clock_mhz`]), or the machine profile's pinned
    /// clock (the A1200/CD32's authentic 14.18 MHz) when the profile names
    /// one; overridable via `[cpu] clock_mhz`.
    pub cpu_clock_mhz: f64,
    /// Model the 68020/030 on-chip instruction cache (CACR-controlled).
    /// Defaults on for the parts that have one (68EC020/68020/68030), as on
    /// real silicon; `[cpu] icache = false` opts a 020/030 back out.
    pub cpu_icache: bool,
    /// Model the 68030 on-chip data cache (CACR-controlled). Defaults on for
    /// the 68030. Only caches expansion RAM and ROM (chip/slow RAM get
    /// cache inhibit, as on real Amigas, because DMA writes them).
    pub cpu_dcache: bool,
    /// 68060 unimplemented-instruction policy (faithful traps by default).
    pub cpu_unimplemented: UnimplementedPolicy,
    /// Fast CPU execution through the m68k core's batch/trace-JIT path
    /// (`[cpu] jit`). Trades the cycle-exact CPU timing model for host
    /// speed: instructions retire in batches at an approximate cost, so
    /// the machine behaves like one with an accelerator card fitted.
    /// Defaults off.
    pub cpu_jit: bool,
    pub emulation: Emulation,
    pub chip_ram_bytes: usize,
    pub fast_ram_bytes: usize,
    pub slow_ram_bytes: usize,
    /// Cold power-on contents for writable system RAM (`[memory] init`). Zero
    /// is the compatibility default; a fixed word or deterministic random
    /// data is an opt-in guest-development aid for exposing uninitialised
    /// reads.
    pub ram_init: RamInit,
    /// Ramsey-controlled motherboard fast RAM (`[memory] motherboard`):
    /// 32-bit local RAM ending at $08000000 and growing downward, sized by
    /// Kickstart's own probe rather than autoconfig. Needs a Ramsey
    /// ([`MemController`]) and a CPU with a 32-bit address bus. The
    /// A3000/A4000 profiles fit 4 MiB by default. Ramsey's four banks stop
    /// at 16 MiB; larger totals (up to 64 MiB, Ramsey-07/A4000 only) fill
    /// the $04000000-$06FFFFFF motherboard RAM expansion space below them.
    pub mb_ram_bytes: usize,
    /// CPU-slot (accelerator) fast RAM (`[memory] accelerator`): 32-bit
    /// local RAM in the big-box coprocessor-slot space, starting at
    /// $08000000 and growing upward (up to 128 MiB, ending where Zorro III
    /// space begins), sized by Kickstart's own probe rather than autoconfig.
    /// Needs a CPU with a 32-bit address bus.
    pub accel_ram_bytes: usize,
    /// Zorro III autoconfig RAM (`[memory] z3`). Needs a CPU with a 32-bit
    /// address bus (68020/030/040; not the 24-bit 68000/68EC020).
    pub z3_ram_bytes: usize,
    /// Extra Zorro RAM boards loaded from `[[zorro]]` metadata files, in
    /// autoconfig chain order after the built-in RAM boards.
    pub zorro_boards: Vec<BoardSpec>,
    /// WASM plugin boards loaded from `[[zorro]]` metadata files. Instantiated
    /// and attached to the bus at machine-build time (their device-slot index
    /// is assigned then); kept separate from RAM boards because they carry a
    /// module and capabilities, not just an autoconfig identity.
    pub wasm_boards: Vec<WasmBoardConfig>,
    /// Advertise the Copperline identification board on the Zorro autoconfig
    /// chain (manufacturer 5192 / product 2) so guest software such as
    /// identify.library can detect the emulator. Defaults to true; set
    /// `identify = false` for a chain with no emulator-identifying board.
    pub identify_board: bool,
    /// `[[filesys]]` host directories exported to the guest as
    /// AmigaDOS devices `HOSTFS0:`, `HOSTFS1:`, ... (experimental). Empty:
    /// no services board on the autoconfig chain.
    pub filesys: Vec<crate::filesys::MountSpec>,
    /// Host-only `--run` program directory. This is not a TOML setting; it is
    /// the authority root for opt-in uaelib file commands.
    #[doc(hidden)]
    pub run_program_dir: Option<PathBuf>,
    /// `[[host_disk]]` real host disks attached to the machine. Empty is the
    /// ordinary case: a machine with no real storage bolted to it.
    pub host_disks: Vec<HostDiskConfig>,
    pub chipset: Chipset,
    /// Concrete chip revisions derived from the `[chipset] revision` preset,
    /// installed chip RAM, and the optional `agnus`/`denise` overrides.
    pub agnus_revision: AgnusRevision,
    pub denise_revision: DeniseRevision,
    /// Selected machine profile, if a `[machine]` section was given.
    pub machine: Option<MachineModel>,
    pub gate_array: GateArray,
    /// Memory controller fitted: a Ramsey on the big-box machines.
    pub mem_controller: MemController,
    /// Log every CPU access that no device decodes, within this address range.
    /// Set by `[debug] log_unmapped`. Off by default: on a booting machine the
    /// ROM probes enough empty space to make this a firehose, so it is meant
    /// to be pointed at one window (e.g. the A4000 IDE at $DD2020).
    pub log_unmapped: Option<std::ops::RangeInclusive<u32>>,
    /// Arm the custom-register access validator and last-writer table.
    /// Set by `[debug] validate_chipset`. Off by default: it is a
    /// diagnostic, and an unarmed machine pays nothing for it.
    pub validate_chipset: bool,
    /// Report self-modifying writes. Set by `[debug] detect_smc`.
    pub detect_smc: bool,
    /// A4000 motherboard IDE fitted (A4000 profile): the ATA task file at
    /// $DD2020, driven by Kickstart's own scsi.device. Takes its drives from
    /// `[ide]`, like Gayle's.
    pub ide_a4000: bool,
    /// Super DMAC fitted (A3000 profile): the SCSI DMA controller at $DD0000
    /// and the WD33C93 behind it. Kickstart hangs outright if nothing answers
    /// there. Drives go on its bus through `[scsi] controller = "a3000"`.
    pub sdmac: bool,
    /// Keep the ROM's scsi.device from initialising. Defaults to set when
    /// the machine's built-in disk controller (Gayle or A4000 IDE, A3000
    /// SDMAC SCSI) has no drives configured: the driver would only cost boot
    /// time probing an empty bus. With drives configured the default is
    /// false and the driver runs -- scsi.device is their boot path. Set by
    /// `[machine] rom_scsi_device_disable`; see [`crate::romtags`].
    pub rom_scsi_device_disable: bool,
    /// Akiko gate array fitted (CD32 profile): ID + C2P port at $B80000.
    pub akiko: bool,
    /// CDTV DMAC/CD controller fitted (CDTV profile): a Zorro II
    /// autoconfig board carrying the 6525 TPI and the Matshita drive.
    pub cdtv_cd: bool,
    /// Extended ROM image (`extended_rom = "path"`): 512 KiB maps at
    /// $E00000 (CD32), 256 KiB at $F00000 (CDTV).
    pub extended_rom_path: Option<PathBuf>,
    /// CD32 Full Motion Video cartridge ROM (`fmv_rom = "path"`). The CD32
    /// profile defaults to Copperline's bundled open ROM; an empty string
    /// explicitly leaves the physical 1 MiB autoconfig module unfitted.
    pub fmv_rom_path: Option<PathBuf>,
    /// CD image (`[cd] image = "disc.cue"`), mounted on the machine's CD
    /// controller (CD32 Akiko or CDTV DMAC).
    pub cd_image_path: Option<PathBuf>,
    /// Emulated seconds after power-on at which the CD is inserted
    /// (0 = present at boot). Some CDTV discs need a post-boot insert.
    pub cd_insert_delay_secs: f64,
    /// CD32 NVRAM backing file (None = session-only EEPROM).
    pub cd32_nvram_path: Option<PathBuf>,
    /// Whether the battery RTC at $DC0000 is fitted. Defaults to false:
    /// the base A500/A500OCS, A600, A1200, A1000, and CD32 shipped without a
    /// battery-backed clock. Only the A500+ (soldered on the Rev 8A board),
    /// the CDTV, and the big-box A3000/A4000 carry one by default; the A600HD
    /// and a clock-equipped A1200 set `[machine] rtc = true`.
    pub rtc_present: bool,
    /// Which clock part answers there (`[machine] rtc_chip`): the OKI
    /// MSM6242 on most boards, the Ricoh RP5C01 on the A3000/A4000 -- a
    /// different register protocol, which battclock.resource probes for
    /// but Linux/m68k assumes from the machine model. Defaults per
    /// profile; setting it implies `rtc = true`.
    pub rtc_chip: crate::rtc::RtcChip,
    /// Power-on RTC value in Unix seconds (`[machine] rtc_time` /
    /// `--rtc-time`). When set, the clock starts here and ticks with
    /// *emulated* time instead of following the host wall clock, so the
    /// guest-visible time is deterministic and reproducible. Setting a time
    /// implies fitting the clock (`rtc = true`).
    pub rtc_seed_unix: Option<u64>,
    /// Stop the seeded RTC (`[machine] rtc_frozen`): every read returns
    /// exactly `rtc_seed_unix`, for pinning a guest to one time window.
    pub rtc_frozen: bool,
    /// RP5C01 battery-RAM (battmem) backing file (`[machine] battmem`),
    /// in the WinUAE/Amiberry `.nvram` layout; `None` keeps the battery
    /// registers session-only. Defaults to `battmem.nvram` when an
    /// RP5C01 is fitted.
    pub battmem_path: Option<PathBuf>,
    pub video_standard: VideoStandard,
    pub audio: AudioConfig,
    /// Gayle IDE drive images (raw flat HDF, RDB inside), opened
    /// read/write. Only valid on machines with a Gayle gate array.
    pub ide: IdeConfig,
    /// SCSI controller (`[scsi]`): the `controller` selects an A2091 (Zorro II),
    /// an A4091 (Zorro III), or the A3000's motherboard SCSI, plus up to seven
    /// drive images on SCSI IDs 0-6. The Zorro boards autoconfig on the chain
    /// and carry their own boot ROM and scsi.device; the A3000's does not.
    pub scsi: ScsiConfig,
    /// Copperline's virtual hardfile controller (`[copperhf]`): up to seven
    /// units of `copperhf.device`, the same drive-descriptor shape as
    /// `[ide]`/`[scsi]`/`[lide]` but with no real-hardware counterpart and
    /// no boot ROM key of its own.
    pub copperhf: CopperhfConfig,
    /// `lide.device`-compatible Zorro II IDE board (`[lide]`): RIPPLE, RIDE,
    /// or AT-Bus 2008, autoconfigs on the chain like the SCSI boards. Drives
    /// may be hard disks or ATAPI CD-ROMs. A fitted board with no ROM named
    /// defaults to the bundled one for its personality (`rom = ""` opts
    /// out into hardware-only mode).
    pub lide: LideConfig,
    /// A2065 Ethernet board (`[a2065]`): when set, an A2065 NIC autoconfigs on
    /// the Zorro chain using the named host network backend. Networking is
    /// non-deterministic, so a fitted A2065 breaks byte-identical replay.
    pub a2065_net: Option<crate::net::NetConfig>,
    /// MacroSystem Toccata sound board (`[toccata] enabled = true`): when
    /// true, a Toccata autoconfigs on the Zorro chain and its AD1848 output
    /// joins the mixer as the `toccata` audio source. No other options
    /// exist yet (see docs/internals/toccata.md).
    pub toccata: bool,
    /// Freezer cartridge (`[cartridge]`, `crate::cartridge`): a system
    /// monitor in its own bank at $A10000, entered by a level-7 interrupt
    /// on a freeze.
    pub cartridge: CartridgeConfig,
    /// The MHI virtual MPEG audio decoder board (`[mhi] enabled = true`):
    /// when true, an MHI board autoconfigs on the Zorro chain and its
    /// decoded-MP3 output joins the mixer as the `mhi` audio source. No
    /// other options exist yet (see docs/internals/mhi.md). Needs a build
    /// with the `mhi` feature (on by default; off only for the wasm32
    /// browser core, which builds `default-features = false`).
    pub mhi: bool,
    /// HostSocket board backend (`[hostsocket] net`): when set, the bundled
    /// bsdsocket.library plugin board is fitted with this backend. The board
    /// itself travels in [`Config::wasm_boards`]; this field records the
    /// resolved backend for surfaces (the launcher) that need to read it
    /// without digging the bundled entry back out of that list.
    pub hostsocket_net: Option<crate::net::NetConfig>,
    /// HostSocket transport (`[hostsocket] net = "host"`): `Some("host")`
    /// when the board bypasses `hostsocket_net`'s smoltcp backend entirely
    /// for direct real-host-socket passthrough (`crate::hostsocket`'s own
    /// doc comment) -- `None` otherwise. `hostsocket_net` alone can't tell
    /// this apart from plain `net = "loopback"`: `"host"` mode still
    /// resolves its underlying smoltcp interface to `Loopback` (ICMP/DNS
    /// still ride it), so this field is the only place that distinction
    /// survives past `Config::from_raw` -- needed so the launcher (and any
    /// other surface reading `Config` back) doesn't silently downgrade a
    /// saved `net = "host"` to `net = "loopback"` on the next round trip.
    pub hostsocket_transport: Option<String>,
    /// RTG graphics card (`[rtg] card`): when set, the card autoconfigs on
    /// the Zorro chain and presents RTG screens (all pixel formats, core
    /// blitter ops, hardware mouse sprite) to its Picasso96 driver.
    pub rtg: RtgCard,
    /// Picasso II/II+ and Graffity display memory. Ignored by the Z3660.
    pub rtg_vram_bytes: usize,
    pub floppy: FloppyConfig,
    /// Which floppy drive slots are electrically present. DF0 is the
    /// internal drive and is always present; DF1-DF3 are external drives
    /// that answer the standard Amiga external-drive ID protocol when true.
    pub floppy_connected: [bool; 4],
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
    /// Where the TV-overscan presentation centres the picture on the
    /// glass, like a monitor's H-CENTER/V-CENTER controls. See
    /// [`TvCentre`].
    pub tv_centre: TvCentre,
    /// Presentation pixel aspect: how emulated scanlines map to host
    /// rows in the window and in screenshots. See [`PixelAspect`].
    pub pixel_aspect: PixelAspect,
    /// How the presentation canvas is scaled into the window: aspect-fit
    /// with filtering, or whole-number multiples only. See
    /// [`DisplayScaling`]. A separate question from `pixel_aspect`, which
    /// decides the shape of the picture; integer scaling draws that shape
    /// from the unresampled canvas under either aspect.
    pub scaling: DisplayScaling,
    /// Crop the window presentation to the display window the hardware
    /// programs (`[display] autocrop`, default off), so a display using
    /// fewer lines than the full scan fills more of the window -- the
    /// Amiberry-style autocrop. A window-presentation setting only:
    /// captures always keep their configured aperture.
    pub autocrop: bool,
    /// Motion-adaptive deinterlacing of LACE content (on by default).
    /// Off, every field is plain line-doubled as it arrives, which shows
    /// interlace bob/flicker like a real TV without persistence.
    pub deinterlace: bool,
    /// CRT phosphor persistence: the fraction of the previous presented
    /// frame each new frame keeps (0.0 = off). Approximates the phosphor
    /// decay that fuses field-rate dither and interlace flicker on a
    /// real CRT.
    pub phosphor: f32,
    /// GPU shader pass applied to the window image. See [`ShaderMode`].
    pub shader: ShaderMode,
    /// How strongly the shader pass is mixed in, 0.0 (invisible) to 1.0
    /// (the preset's full effect, the default). A single knob for every
    /// preset so the effect can be dialled back without editing shaders.
    pub shader_strength: f32,
    /// Which monitor front the window frames the picture with at start
    /// (`[display] bezel`). The `Cmd+M` / `Alt+M` toggle turns it off and
    /// back on live without affecting this start-up value.
    pub bezel: BezelStyle,
    /// Where this machine's output goes and where its file dialogs open
    /// (`[paths]`). Empty until somebody moves something; put in force by
    /// [`crate::paths::adopt`], which drops whatever this host cannot
    /// reach so a configuration written elsewhere still starts.
    pub paths: crate::pathconf::Paths,
    /// Folder of PNG stickers drawn onto the monitor bezel
    /// (`[display] bezel_stickers`). Each PNG in the folder becomes a
    /// decal on the drawn front; an optional `stickers.toml` in the folder
    /// places each one, and without it they line up along the cabinet's
    /// top band. Drawn only while a bezel is; never in captures.
    pub bezel_stickers: Option<PathBuf>,
    /// Show the performance overlay at start (`[display] perf_overlay`, or
    /// `--perf-overlay`): a live emulation-performance readout in the
    /// top-right of the display. The `Cmd+P` / `Alt+P` toggle flips it live
    /// without affecting this start-up value.
    pub perf_overlay: bool,
    /// Screen tint applied to the window image: the phosphor colour of a
    /// monochrome monitor, or a sepia treatment. See [`Tint`].
    pub tint: Tint,
    /// How large the pop-up menu is drawn (`[display] menu_scale`).
    pub menu_scale: MenuScale,
    /// Open the window in fullscreen at start (`[display] full_screen`, or
    /// `--full-screen` / `--windowed`). The `Cmd+F` / `Alt+F` toggle flips it
    /// live without affecting this start-up value.
    pub full_screen: bool,
    /// Show the status bar at start (`[display] status_bar`, or
    /// `--show-status-bar` / `--hide-status-bar`). `Cmd+Shift+F` /
    /// `Alt+Shift+F` toggles it live.
    pub status_bar: bool,
    /// Initial host input source for the emulated joystick/CD32-pad port
    /// (`[input] joystick` / `--joystick`). Defaults to
    /// [`JoystickInputMode::Gamepad`]; the runtime status-bar toggle, `Cmd+J` /
    /// `Alt+J`, and the menu's Joystick Input item flip it live without
    /// affecting this start-up value.
    pub joystick_input_mode: JoystickInputMode,
    /// Host mouse sensitivity, 0-100 (`[input] mouse_sensitivity` /
    /// `--mouse-sensitivity`). 50 (default) is 1:1 with the host mouse; 0 is a
    /// quarter speed and 100 quadruple, on an exponential scale. A host-input
    /// scale only -- it does not affect the emulated machine or scripted mouse
    /// input.
    pub mouse_sensitivity: u8,
    /// When the host mouse is grabbed (`[input] mouse_capture` /
    /// `--mouse-capture`). Defaults to [`MouseCapture::Click`], the
    /// historical click-the-display behaviour; `auto` grabs on focus, which
    /// suits a fullscreen session where no host cursor is wanted.
    pub mouse_capture: MouseCapture,
    /// `[input] autofire_hz`: how fast a held fire button is pulsed, or 0 for
    /// off (the default). A host input convenience, not machine state -- the
    /// emulated port sees an ordinary button being pressed and released.
    pub autofire_hz: u8,
    /// Controller devices plugged into the two game ports at power-on
    /// (`[input] port1` / `port2`, `--port1` / `--port2`); index 0 = port 1.
    /// Defaults to a mouse in port 1 and a joystick in port 2 -- a CD32
    /// joypad on the CD32 profile, whose serial button protocol
    /// lowlevel.library expects. Runtime hot-plug (menu, CCP
    /// `input.set_port`) changes the live machine without affecting this
    /// start-up value.
    pub port_devices: [PortDevice; 2],
    /// Host wiring for Paula's serial port (`[serial]` / `--serial`).
    /// Defaults to [`SerialMode::Stdout`], preserving the historical
    /// terminal-diagnostics behaviour.
    pub serial: SerialConfig,
    /// The peripheral on the Centronics parallel port (printer capture or audio
    /// sampler) and its settings. [`ParallelDevice::None`] leaves the port
    /// electrically disconnected, so CIA-A strobes receive no FLAG acknowledge
    /// and port-B reads see the CIA's own pin state.
    pub parallel: ParallelConfig,
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
    /// Mask the deep horizontal overscan margins with black, like a CRT
    /// bezel, while preserving vertical border colour changes. Demos often
    /// leave junk in the deep horizontal overscan (e.g. HAM streams
    /// converging off-screen); a real TV hides it behind the bezel, and so
    /// does this mode. The default.
    #[default]
    Tv,
}

/// TV-presentation centring (`[display] tv_h_centre` / `tv_v_centre`),
/// the H-CENTER/V-CENTER controls a monitor carries on its front: nudge
/// where the TV aperture sits on the raster. `h` is in lo-res pixels,
/// positive moving the picture right (revealing the left overscan, where
/// the capture holds more raster than the default aperture shows); `v` is
/// in scan lines, positive moving the picture down. Glass the nudged
/// aperture exposes beyond the captured raster is unscanned and shows
/// black, as past the raster's edge on a real tube. Applies to the TV
/// overscan presentation only -- window and captures alike -- since the
/// full-overscan view already presents everything.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TvCentre {
    /// Lo-res pixels, positive right, `-TV_H_CENTRE_RANGE..=TV_H_CENTRE_RANGE`.
    pub h: i32,
    /// Scan lines, positive down, `-TV_V_CENTRE_RANGE..=TV_V_CENTRE_RANGE`.
    pub v: i32,
}

/// Centring ranges: at +16 the aperture's left edge reaches (nearly) the
/// TV bezel mask, the most overscan the TV presentation models a tube
/// showing; the vertical range likewise spans the captured overscan rows.
/// A knob past these would only pull more unscanned black onto the glass.
pub const TV_H_CENTRE_RANGE: i32 = 16;
pub const TV_V_CENTRE_RANGE: i32 = 8;

/// How emulated scanlines map to host rows in the window and in
/// screenshots. The `COPPERLINE_PIXEL_ASPECT` env var (tv/square)
/// overrides the config for one run.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum PixelAspect {
    /// Present the field with the non-square pixel aspect of a 4:3 CRT:
    /// the full overscan scan maps onto a 4:3 output, so PAL lo-res
    /// pixels come out slightly wider than tall, exactly as a real TV
    /// shows them. The default.
    #[default]
    Tv,
    /// Present with square pixels: one host row per woven scanline, so a
    /// standard lo-res display is an exact 2x2 of its 320-wide bitmap
    /// (e.g. 320x256 PAL occupies precisely 640x512 window pixels).
    /// Slightly taller than a real 4:3 CRT picture, but every pixel is
    /// an integer square, which suits side-by-side pixel comparisons.
    Square,
}

/// How the presentation canvas is scaled into the host window
/// (`[display] scaling`). A window-presentation setting only: the
/// framebuffer, screenshots, frame dumps and recordings are the canvas
/// itself and never see it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum DisplayScaling {
    /// Fit the canvas to the window preserving its aspect ratio, with
    /// linear filtering -- the whole window height (or width) is used
    /// whatever the ratio works out to. The default.
    #[default]
    Smooth,
    /// Draw the canvas at whole-number multiples, centred in black borders
    /// and point-sampled, so every canvas pixel is the same block of host
    /// pixels. Always from the unresampled (square) canvas: the largest
    /// uniform multiple under the square aspect, and under the tv aspect a
    /// separate whole number per axis -- pixels per column and per scan
    /// line -- chosen to approximate the 4:3 pixel shape
    /// (`video::window::present::per_axis_fit`). Falls back to the smooth
    /// fit when the window is too small for even 1x, which shrinks rather
    /// than crops.
    Integer,
}

impl DisplayScaling {
    /// Every mode, in the order a picker offers them.
    pub const MENU_ORDER: [DisplayScaling; 2] = [DisplayScaling::Smooth, DisplayScaling::Integer];

    /// Picker label: the config name of the mode (round-trips through
    /// [`parse_display_scaling`]).
    pub fn label(self) -> &'static str {
        match self {
            DisplayScaling::Smooth => "Smooth",
            DisplayScaling::Integer => "Integer",
        }
    }
}

/// The GPU shader pass the window applies to the presented image. The
/// `COPPERLINE_SHADER` env var overrides the config for one run. A
/// presentation stage only: screenshots, frame dumps, recordings and
/// headless runs never see it, so captures stay comparable whatever is
/// selected here.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum ShaderMode {
    /// Present the deinterlaced image untouched. The default. Spelled
    /// "none" or "off" in the config.
    #[default]
    None,
    /// Darken between the emulated scan lines -- one dark band per
    /// emulated line whatever the window scale, the line structure a
    /// 15 kHz CRT leaves.
    Scanlines,
    /// Modulate the output through a staggered RGB dot/shadow mask,
    /// like a slot-mask consumer tube.
    Mask,
    /// Scanlines and an aperture-grille phosphor mask together with
    /// tube curvature and a corner vignette: the full CRT look.
    Crt,
    /// A user WGSL fragment shader loaded from this path at start-up.
    Custom(PathBuf),
}

impl ShaderMode {
    /// The mode without its custom path, for callers that only name the
    /// selection (menu labels, status text).
    pub fn kind(&self) -> ShaderKind {
        match self {
            ShaderMode::None => ShaderKind::None,
            ShaderMode::Scanlines => ShaderKind::Scanlines,
            ShaderMode::Mask => ShaderKind::Mask,
            ShaderMode::Crt => ShaderKind::Crt,
            ShaderMode::Custom(_) => ShaderKind::Custom,
        }
    }
}

/// A [`ShaderMode`] stripped of its custom-shader path, so it is `Copy`
/// and can sit in the `Copy` label structs the menu and status bar build.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShaderKind {
    None,
    Scanlines,
    Mask,
    Crt,
    Custom,
}

impl ShaderKind {
    /// Every shader, in the order a picker offers them. `Custom` is last
    /// because it is the one that depends on a file being configured.
    pub const MENU_ORDER: [ShaderKind; 5] = [
        ShaderKind::None,
        ShaderKind::Scanlines,
        ShaderKind::Mask,
        ShaderKind::Crt,
        ShaderKind::Custom,
    ];

    /// Picker label: the config name of the preset (round-trips through
    /// [`parse_shader`], which takes "off" as well as "none"), or
    /// "custom" for a user shader, whose path is too long to name here.
    pub fn label(self) -> &'static str {
        match self {
            ShaderKind::None => "off",
            ShaderKind::Scanlines => "scanlines",
            ShaderKind::Mask => "mask",
            ShaderKind::Crt => "crt",
            ShaderKind::Custom => "custom",
        }
    }

    /// What a picker shows the user, as against the config name [`label`]
    /// round-trips. Both pickers read this, so they cannot drift apart.
    ///
    /// [`label`]: ShaderKind::label
    pub fn menu_label(self) -> &'static str {
        match self {
            ShaderKind::None => "Disabled",
            ShaderKind::Scanlines => "Scanlines",
            ShaderKind::Mask => "Mask",
            // Named for the monitor the preset is modelled on; the path of a
            // user shader is too long for a value column.
            ShaderKind::Crt => "CRT (1084)",
            ShaderKind::Custom => "Custom",
        }
    }
}

/// Which monitor front the window frames the picture with (`[display]
/// bezel`): the display shrinks into the rounded opening of a procedural
/// frame drawn on the GPU. Independent of [`ShaderKind`], and a
/// presentation stage like it: screenshots, frame dumps, recordings and
/// headless runs never include the bezel.
///
/// A style is a shader source plus the opening it leaves, so adding one is
/// a `.wgsl` file beside the others and an arm here; nothing outside
/// `video::window::bezel` needs to learn its name.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum BezelStyle {
    /// No frame; the picture fills the display rect. The default. Spelled
    /// "none" or "off" in the config, and `false` still works.
    #[default]
    None,
    /// The 1084 the Amiga shipped with: a two-tone cabinet with the tube
    /// sunk into its moulding, model badge, logotype and power lamp.
    Model1084,
    /// The plain rounded frame Copperline drew before the 1084 arrived.
    Classic,
}

impl BezelStyle {
    /// Every style, in the order a picker offers them.
    pub const MENU_ORDER: [BezelStyle; 3] =
        [BezelStyle::None, BezelStyle::Model1084, BezelStyle::Classic];

    /// Config name, which round-trips through [`parse_bezel`].
    pub fn label(self) -> &'static str {
        match self {
            BezelStyle::None => "off",
            BezelStyle::Model1084 => "1084",
            BezelStyle::Classic => "classic",
        }
    }

    /// What a picker shows the user. Both pickers read this, so they
    /// cannot drift apart.
    pub fn menu_label(self) -> &'static str {
        match self {
            BezelStyle::None => "Disabled",
            BezelStyle::Model1084 => "1084",
            BezelStyle::Classic => "Classic",
        }
    }

    /// Whether a frame is drawn at all.
    pub fn is_on(self) -> bool {
        self != BezelStyle::None
    }
}

/// Screen tint the window applies to the presented chipset display: a
/// monochrome-monitor phosphor look or a sepia treatment, matching the web
/// front-end's screen filter. The `COPPERLINE_TINT` env var overrides the
/// config for one run. A presentation stage only, like [`ShaderMode`]:
/// screenshots, frame dumps, recordings and headless runs are never
/// tinted, so captures stay comparable whatever is selected here. RTG
/// board scanout is presented untinted too: the tint models the monitor
/// on the Amiga's video output.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Tint {
    /// Full colour, untinted. The default. Spelled "none" or "off" in the
    /// config.
    #[default]
    None,
    /// Black and white: luminance only, like a mono composite feed.
    Bw,
    /// Green phosphor, the classic P1 monochrome monitor look.
    Green,
    /// Amber phosphor, the other common monochrome monitor look.
    Amber,
    /// Sepia-toned monochrome.
    Sepia,
}

/// How large the pop-up menu is drawn. The panel font is a bitmap, so the
/// sizes are whole multiples of it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MenuScale {
    /// The font at its own size. The default.
    #[default]
    Normal,
    /// Twice up, for a large display or a distant seat.
    Large,
}

impl MenuScale {
    /// Every size, in the order a picker offers them.
    pub const MENU_ORDER: [MenuScale; 2] = [MenuScale::Normal, MenuScale::Large];

    /// Picker label: the config name of the size (round-trips through
    /// [`parse_menu_scale`]).
    pub fn label(self) -> &'static str {
        match self {
            MenuScale::Normal => "1x",
            MenuScale::Large => "2x",
        }
    }

    /// What a picker with room shows: the name and the figure together. The
    /// menu itself shows [`label`] alone, having no width to spare.
    ///
    /// [`label`]: MenuScale::label
    pub fn menu_label(self) -> &'static str {
        match self {
            MenuScale::Normal => "Normal (1x)",
            MenuScale::Large => "Large (2x)",
        }
    }

    /// What every length in the menu is multiplied by.
    pub fn factor(self) -> usize {
        match self {
            MenuScale::Normal => 1,
            MenuScale::Large => 2,
        }
    }
}

impl Tint {
    /// Every tint, in the order a picker offers them.
    pub const MENU_ORDER: [Tint; 5] = [Tint::None, Tint::Bw, Tint::Green, Tint::Amber, Tint::Sepia];

    /// Picker label: the config name of the tint (round-trips through
    /// [`parse_tint`], which takes "off" as well as "none").
    pub fn label(self) -> &'static str {
        match self {
            Tint::None => "off",
            Tint::Bw => "bw",
            Tint::Green => "green",
            Tint::Amber => "amber",
            Tint::Sepia => "sepia",
        }
    }

    /// What a picker shows the user, as against the config name [`label`]
    /// round-trips. "Colour" rather than "Off": it says what the picture
    /// looks like, and it is the web front-end's wording for the same picker.
    ///
    /// [`label`]: Tint::label
    pub fn menu_label(self) -> &'static str {
        match self {
            Tint::None => "Colour",
            Tint::Bw => "Black & white",
            Tint::Green => "Green",
            Tint::Amber => "Amber",
            Tint::Sepia => "Sepia",
        }
    }
}

/// Host input source for the emulated port-2 joystick/CD32 pad. `Gamepad` (the
/// default) uses only a physical pad, so the keyboard passes straight through to
/// the Amiga (and with no pad connected there is simply no port-2 input).
/// `Keyboard` always uses the keyboard-joystick mapping, capturing the arrow
/// keys and fire keys. There are deliberately only these two explicit modes: the
/// status-bar toggle and `Cmd+J` / `Alt+J` flip between them, so the active mode
/// is always visible rather than depending on hidden gamepad-presence state. Set
/// the start-up mode with `[input] joystick` (or `--joystick`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum JoystickInputMode {
    #[default]
    Gamepad,
    Keyboard,
}

impl JoystickInputMode {
    /// Flip the two-state toggle (status bar, `Cmd+J`/`Alt+J`, launcher stepper).
    pub fn next(self) -> Self {
        match self {
            Self::Gamepad => Self::Keyboard,
            Self::Keyboard => Self::Gamepad,
        }
    }

    /// Short label for menus, the on-screen flash, and the config string
    /// (round-trips through [`parse_joystick_input_mode`]).
    pub fn label(self) -> &'static str {
        match self {
            Self::Gamepad => "gamepad",
            Self::Keyboard => "keyboard",
        }
    }

    /// What a picker shows the user, as against the config name [`label`]
    /// round-trips.
    ///
    /// [`label`]: JoystickInputMode::label
    pub fn menu_label(self) -> &'static str {
        match self {
            Self::Gamepad => "Gamepad",
            Self::Keyboard => "Keyboard",
        }
    }
}

/// When the host mouse is grabbed: the pointer is confined to the window
/// and the host cursor hidden, so the emulated mouse is the only one on
/// screen (`[input] mouse_capture` / `--mouse-capture`).
///
/// Uncaptured, host cursor motion over the display still drives the
/// emulated mouse; this setting only decides when the grab is taken, not
/// whether motion reaches the machine. `Cmd+G` / `Alt+G` releases and
/// re-takes it by hand in every mode.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum MouseCapture {
    /// Clicking the display takes the grab (the default). That click is a
    /// window action and is not passed to the Amiga.
    #[default]
    Click,
    /// Grab as soon as the window has the focus, and again whenever it
    /// regains it, so there is never a host cursor loose over the display.
    Auto,
    /// Only the shortcut grabs. Clicks on the display go straight to the
    /// Amiga with the host cursor left alone.
    Manual,
}

impl MouseCapture {
    /// Short label for menus and the config string (round-trips through
    /// [`parse_mouse_capture`]).
    pub fn label(self) -> &'static str {
        match self {
            Self::Click => "click",
            Self::Auto => "auto",
            Self::Manual => "manual",
        }
    }
}

/// Where Paula's serial port is wired on the host (`[serial] mode` /
/// `--serial`). The Amiga serial port is also the MIDI port, so the MIDI
/// backend is one of these modes rather than a separate device.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum SerialMode {
    /// Serial output is discarded and there is no serial input.
    Off,
    /// Serial output is written to the host terminal. The historical
    /// default (DiagROM and other tools print diagnostics here), kept as
    /// the default so an unconfigured machine behaves exactly as before.
    #[default]
    Stdout,
    /// Serial in/out is bridged to host MIDI endpoints. Requires a build
    /// with the `midi` feature; without it, resolving this mode is an error.
    Midi,
    /// Serial in/out is bridged to a host TCP port, like UAE's `TCP:`
    /// serial device. With an `AUX:` shell on the Amiga side, a connected
    /// client gets a remote AmigaDOS console.
    Tcp,
    /// Serial in/out dials out to a remote TCP service at startup (the
    /// address in [`SerialConfig::connect`]): a telnet BBS, a `tcpser`
    /// modem bridge, a `socat`-exposed device. The outbound counterpart
    /// of [`Tcp`].
    ///
    /// [`Tcp`]: SerialMode::Tcp
    TcpConnect,
    /// Serial in/out is bridged to a host pseudo-terminal. The emulator
    /// allocates a pty and logs the slave path (`/dev/pts/N`); a terminal
    /// program (`minicom`, `screen`, `cu`) attaches to it. Unix hosts only.
    Pty,
    /// Hayes AT-command modem personality over TCP: the guest dials out
    /// with `ATD` rather than the port being wired to a peer at startup,
    /// so unlike [`Tcp`]/[`TcpConnect`] there is no connect address in the
    /// config.
    ///
    /// [`Tcp`]: SerialMode::Tcp
    /// [`TcpConnect`]: SerialMode::TcpConnect
    Modem,
}

impl SerialMode {
    /// Config-string label (round-trips through [`parse_serial_mode`]).
    pub fn label(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Stdout => "stdout",
            Self::Midi => "midi",
            Self::Tcp => "tcp",
            Self::TcpConnect => "tcp-connect",
            Self::Pty => "pty",
            Self::Modem => "modem",
        }
    }
}

/// Resolved `[serial]` settings. `midi_out`/`midi_in` name the host MIDI
/// endpoints (substring match) and are only consulted when `mode` is
/// [`SerialMode::Midi`]; they are carried through in the other modes so the
/// configuration screen round-trips them unchanged.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SerialConfig {
    pub mode: SerialMode,
    pub midi_out: Option<String>,
    pub midi_in: Option<String>,
    /// The MT-32's two ROM images. Not Copperline's to ship, so the user
    /// supplies them; without both, `midi_out = "mt32"` has nothing to fit
    /// and says so.
    pub mt32_control_rom: Option<PathBuf>,
    pub mt32_pcm_rom: Option<PathBuf>,
    /// Show the MT-32's front panel under the status bar (default false).
    pub mt32_panel: bool,
    /// How that panel's display is lit.
    pub mt32_lcd: Mt32Lcd,
    /// Coppersynth's soundfont (.sf2, or a .zip holding one). A path
    /// option like the ROMs above: the bundled default's search path is
    /// consulted when the device is attached, not here, so a config that
    /// never selects the synth never demands the file.
    pub coppersynth_soundfont: Option<PathBuf>,
    /// Coppersynth's MT-32 mode: "auto" (default), "on", or "off".
    pub coppersynth_mt32_mode: Option<String>,
    /// Show Coppersynth's front panel under the status bar (default false).
    pub coppersynth_panel: bool,
    /// TCP listen address for [`SerialMode::Tcp`]; `None` means
    /// [`SERIAL_TCP_DEFAULT_LISTEN`].
    pub listen: Option<String>,
    /// Remote `host:port` for [`SerialMode::TcpConnect`]. Required in that
    /// mode (there is no sensible default host to dial).
    pub connect: Option<String>,
    /// `AT*T1`/`AT*T0` default at power-on: telnet NVT translation on by
    /// default. [`SerialMode::Modem`] only. Tri-state on purpose: `None` is
    /// "the config never said", which defers to a stored `AT&W` profile's
    /// own `AT*T`, while `Some(false)` is an explicit off that overrides
    /// that profile the same way `Some(true)` overrides it -- collapsing
    /// the two here would silently let a profile's `telnet = true` win over
    /// a config that plainly asked for it off.
    pub telnet: Option<bool>,
    /// `[serial.phonebook]` entries, number -> "host:port" (or a bare host),
    /// sorted by number. [`SerialMode::Modem`] only.
    pub phonebook: Vec<(String, String)>,
    /// `[serial] session`: a scripted session file the modem replays
    /// instead of dialing out over TCP. [`SerialMode::Modem`] only; `None`
    /// means the ordinary TCP transport.
    pub session: Option<PathBuf>,
}

/// Where [`SerialMode::Tcp`] listens with no `[serial] listen` of its own:
/// the loopback interface on the port UAE's `TCP:` serial device uses, so a
/// terminal pointed at either lands in the same place.
pub const SERIAL_TCP_DEFAULT_LISTEN: &str = "127.0.0.1:1234";

/// The host half a serial address defaults to when only a port is typed.
pub const SERIAL_DEFAULT_HOST: &str = "127.0.0.1";
/// The port half a serial address defaults to when only a host is typed --
/// also the port of [`SERIAL_TCP_DEFAULT_LISTEN`].
pub const SERIAL_DEFAULT_PORT: u16 = 1234;

/// How the MT-32's front-panel display is lit.
///
/// The engine draws the same twenty characters whichever this is; only the
/// glass and the characters on it change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Mt32Lcd {
    /// The unit's own: a dark green backlight under lighter green
    /// characters. The default, since this is an MT-32. Unlit it goes to
    /// bare glass, a shade off the surround around it.
    #[default]
    Mt32,
    /// A Super JV's: deep blue under pale green characters. Unlit the
    /// blue stays, a shade darker, as that panel does.
    SuperJv,
    /// An S Series sampler's: a sky-blue backlight with the characters
    /// almost black on it. Unlit it keeps a paler blue, plainly a lamp
    /// gone out rather than a dark screen.
    SSeries,
    /// Black glass under the green the status bar's track counter uses, as
    /// one of the OLED panels sold to replace a tired original looks.
    Oled,
}

impl Mt32Lcd {
    /// Every style, in the order a picker offers them.
    pub const MENU_ORDER: [Mt32Lcd; 4] = [
        Mt32Lcd::Mt32,
        Mt32Lcd::SuperJv,
        Mt32Lcd::SSeries,
        Mt32Lcd::Oled,
    ];

    /// Config name, which round-trips through [`parse_mt32_lcd`].
    pub fn label(self) -> &'static str {
        match self {
            Mt32Lcd::Oled => "oled",
            Mt32Lcd::Mt32 => "mt32",
            Mt32Lcd::SuperJv => "superjv",
            Mt32Lcd::SSeries => "sseries",
        }
    }

    /// What a picker shows.
    pub fn menu_label(self) -> &'static str {
        match self {
            Mt32Lcd::Oled => "OLED",
            Mt32Lcd::Mt32 => "MT-32",
            Mt32Lcd::SuperJv => "Super JV",
            Mt32Lcd::SSeries => "S Series",
        }
    }
}

/// Parse a `[serial] mt32_lcd` value. The numbers are taken as well as the
/// names, since the styles are as often thought of as first, second, third.
pub(crate) fn parse_mt32_lcd(s: &str) -> Result<Mt32Lcd> {
    match s.trim().to_ascii_lowercase().as_str() {
        "mt32" | "mt-32" | "1" => Ok(Mt32Lcd::Mt32),
        "superjv" | "super-jv" | "2" => Ok(Mt32Lcd::SuperJv),
        "sseries" | "s-series" | "3" => Ok(Mt32Lcd::SSeries),
        "oled" | "4" => Ok(Mt32Lcd::Oled),
        other => bail!(
            "[serial] mt32_lcd must be \"mt32\", \"superjv\", \"sseries\" \
             or \"oled\" (or 1, 2, 3, 4), got \"{other}\""
        ),
    }
}

/// The `[serial] midi_out` value that means the built-in MT-32 rather
/// than a host endpoint. Matched whole and case-insensitively, so a host
/// device whose name merely contains it is still reachable.
pub const MIDI_OUT_MT32: &str = "mt32";

/// Whether a `midi_out` value asks for the built-in MT-32.
pub fn midi_out_is_mt32(midi_out: Option<&str>) -> bool {
    midi_out.is_some_and(|name| name.trim().eq_ignore_ascii_case(MIDI_OUT_MT32))
}

/// `midi_out = "coppersynth"` selects the built-in Coppersynth synthesizer rather
/// than a host endpoint. Matched like [`MIDI_OUT_MT32`].
pub const MIDI_OUT_CSYNTH: &str = "coppersynth";

/// Whether a `midi_out` value asks for the built-in Coppersynth synth.
pub fn midi_out_is_csynth(midi_out: Option<&str>) -> bool {
    midi_out.is_some_and(|name| name.trim().eq_ignore_ascii_case(MIDI_OUT_CSYNTH))
}

/// Which peripheral is plugged into the Amiga's Centronics parallel port. The
/// port carries one device at a time, chosen by `[parallel] device`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ParallelDevice {
    /// Nothing plugged in (an unplugged cable). The default.
    #[default]
    None,
    /// A Centronics printer whose raw byte stream is captured to a host file
    /// (`[parallel] output`).
    Printer,
    /// An 8-bit audio sampler (digitizer) on the data lines, fed from a host
    /// capture device. Needs a build with the `frontend` feature (cpal).
    Sampler,
}

impl ParallelDevice {
    /// Config-string label (round-trips through [`parse_parallel_device`]).
    pub fn label(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Printer => "printer",
            Self::Sampler => "sampler",
        }
    }
}

/// Resolved `[parallel]` settings. `printer_output` is consulted only for
/// [`ParallelDevice::Printer`], and `sampler_input`/`sampler_gain_db` only for
/// [`ParallelDevice::Sampler`]; the inactive fields are carried through so the
/// configuration screen round-trips them unchanged.
#[derive(Debug, Clone, PartialEq)]
pub struct ParallelConfig {
    pub device: ParallelDevice,
    /// Raw printer-byte capture path for [`ParallelDevice::Printer`].
    pub printer_output: Option<PathBuf>,
    /// Host capture device for [`ParallelDevice::Sampler`]; `None` = default.
    pub sampler_input: Option<String>,
    /// Sampler input gain in decibels (preamp); 0 dB = unity.
    pub sampler_gain_db: f32,
}

impl Default for ParallelConfig {
    fn default() -> Self {
        Self {
            device: ParallelDevice::None,
            printer_output: None,
            sampler_input: None,
            sampler_gain_db: 0.0,
        }
    }
}

/// A configured hard-drive image: the host path plus an optional volume-name
/// override. The override only changes a host *directory* mounted as an
/// in-memory FFS/OFS volume -- it sets the volume label instead of deriving
/// it from the directory name. A raw HDF carries its own label inside the
/// image and ignores the override.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DriveImage {
    pub path: PathBuf,
    pub volume_name: Option<String>,
    /// `de_BootPri` for the partition Copperline synthesizes in front of a
    /// bare hardfile. Only reaches the guest for such images: an HDF that
    /// carries its own RDB keeps the priorities recorded inside it.
    pub boot_pri: i8,
    /// The filesystem an in-memory directory-mount volume is built with.
    /// Only meaningful for a host *directory* mount (an HDF/gzip image
    /// already carries its own filesystem). Defaults to FFS; Kickstart
    /// 1.3's ROM has no FFS handler built in, so OFS is the choice for a
    /// directory mount meant to work under 1.3 with no guest-side setup.
    pub filesystem: crate::diskimage::FileSystem,
}

impl Default for DriveImage {
    fn default() -> Self {
        DriveImage {
            path: PathBuf::new(),
            volume_name: None,
            boot_pri: HARDFILE_DEFAULT_BOOT_PRI,
            filesystem: crate::diskimage::FileSystem::FFS,
        }
    }
}

/// Priority a synthesized hard-disk partition boots at when the config says
/// nothing, matching what HDToolBox writes for a plain hard-disk partition.
/// Kickstart's own DF0: boot node sits at 5, so a hard disk loses the tie to a
/// bootable floppy unless it is raised.
pub const HARDFILE_DEFAULT_BOOT_PRI: i8 = 0;

/// `de_BootPri` value that mounts a partition without offering it for boot,
/// the same sentinel `[[filesys]] bootpri` uses.
pub const BOOT_PRI_NEVER: i8 = -128;

/// Whether a drive-image path names a CD image (a cue sheet, a bare ISO,
/// a Nero NRG, or a CHD). On the SCSI bus such an entry attaches a CD-ROM drive
/// instead of a hard disk; the file extension is the format signal,
/// exactly as it is for the hard-drive back ends (HDF vs. directory).
pub fn is_cd_image_path(path: &std::path::Path) -> bool {
    path.extension().and_then(|e| e.to_str()).is_some_and(|e| {
        e.eq_ignore_ascii_case("cue")
            || e.eq_ignore_ascii_case("iso")
            || e.eq_ignore_ascii_case("nrg")
            || e.eq_ignore_ascii_case("chd")
    })
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IdeConfig {
    pub master: Option<DriveImage>,
    pub slave: Option<DriveImage>,
}

/// Which RTG graphics card the `[rtg]` section fits. A machine has at most
/// one: RTG screens come from whichever card the P96 driver finds, so a
/// second board would only compete for the display.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum RtgCard {
    /// No RTG board; the chipset drives the display. The default.
    #[default]
    None,
    /// The Z3660 accelerator's FPGA RTG core, driven by the open-source
    /// Z3660.card Picasso96 driver.
    Z3660,
    /// Village Tronic Picasso II: Zorro II, CL-GD5426, 1 or 2 MB VRAM.
    Picasso2,
    /// Village Tronic Picasso II+: Zorro II, CL-GD5428, 1 or 2 MB VRAM,
    /// with vertical blank wired to INT2.
    Picasso2Plus,
    /// Atéo Concepts Graffity [Zorro II]: CL-GD5428, 1 or 2 MB VRAM, a
    /// chained VRAM + register aperture pair like Picasso II's.
    GraffityZ2,
    /// Atéo Concepts Graffity [Zorro III]: CL-GD5428, 1 or 2 MB VRAM, one
    /// 16 MB autoconfig window.
    GraffityZ3,
}

/// Which SCSI host adapter the `[scsi]` section fits: one of the two Zorro
/// autoconfig boards, which carry their own boot ROM and scsi.device, or the
/// A3000's motherboard SCSI, which has neither (Kickstart's own scsi.device
/// drives it) and is only there on a machine with a Super DMAC.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ScsiController {
    /// Commodore A2091/A590: Zorro II, WD33C93. The default.
    #[default]
    A2091,
    /// Commodore A4091: Zorro III, NCR 53C710.
    A4091,
    /// A3000 motherboard SCSI: Super DMAC + WD33C93 at $DD0000. The default on
    /// a machine that has one.
    A3000,
}

impl ScsiController {
    /// Whether the controller is a Zorro board (it autoconfigs and needs a boot
    /// ROM) rather than motherboard silicon.
    pub fn is_zorro_board(self) -> bool {
        !matches!(self, ScsiController::A3000)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScsiConfig {
    /// Which host adapter the section fits (`controller`). Only meaningful
    /// when `enabled()`.
    pub controller: ScsiController,
    /// Boot ROM image. For the A2091's split even/odd EPROM dumps, `rom` is
    /// the even half and `rom_odd` the other; the A4091 has a single ROM.
    pub rom: Option<PathBuf>,
    /// Odd-byte EPROM half for split A2091 dumps.
    pub rom_odd: Option<PathBuf>,
    /// Drive images by SCSI ID (0-6; ID 7 is the controller).
    pub units: [Option<DriveImage>; 7],
}

impl ScsiConfig {
    /// Whether a `[scsi]` section asked for a board at all. A bare
    /// `controller` with no ROM or drives fits nothing -- except an A2091 or
    /// A4091, which validation gives a bundled ROM, so `rom` is then set.
    pub fn enabled(&self) -> bool {
        self.rom.is_some() || self.units.iter().any(Option::is_some)
    }
}

/// `[copperhf]`: Copperline's own virtual hardfile controller
/// (`copperhf.device`), analogous to WinUAE's `uaehf.device` -- an
/// emulator-only board with no real-hardware counterpart, moving data at
/// zero emulated cost instead of modelling a real chip's register timing.
/// RDB and RDB-less images are handled identically to `[ide]`/`[scsi]`/
/// `[lide]` through the shared hardfile layer. Guest-visible as
/// `copperhf.device` units 0-6; see `docs/internals/copperhf.md` for the
/// register-level protocol.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CopperhfConfig {
    /// Drive images by unit number (0-6).
    pub units: [Option<DriveImage>; 7],
}

impl CopperhfConfig {
    /// Whether a `[copperhf]` section configured any unit at all.
    pub fn enabled(&self) -> bool {
        self.units.iter().any(Option::is_some)
    }
}

/// `[lide]`: a built-in Zorro II IDE board compatible with LIV2's
/// `lide.device`. Drives may be hard disks or ATAPI CD-ROMs. A fitted board
/// with no ROM named defaults to the bundled ROM for its personality
/// (`lide.rom` for RIPPLE/RIDE, `lide-atbus.rom` for AT-Bus 2008, plus
/// `cdfs.rom` on RIPPLE/RIDE) -- `rom = ""` opts out and keeps the board in
/// hardware-only mode.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LideConfig {
    /// Which of the three AutoConfig identities the board presents. Only
    /// meaningful when `enabled()`.
    pub board: crate::ide_zorro::LidePersonality,
    /// Whether the raw `[lide]` table named a `board` explicitly. A board
    /// can be selected with no ROM and no drive images yet (hardware-only
    /// mode, or a board meant to carry only a `[[host_disk]]` attachment),
    /// so this is tracked separately from `rom`/`drives` rather than left
    /// for `enabled()` to infer from them.
    pub board_named: bool,
    /// Boot ROM image (32768 bytes). Absent = hardware-only mode: no
    /// DiagArea, no autoboot; drives still work under a disk-loaded
    /// `lide.device`.
    pub rom: Option<PathBuf>,
    /// Optional second flash bank (e.g. a CD filesystem image). Requires
    /// `rom`; not valid on the AT-Bus 2008 personality.
    pub rom_bank2: Option<PathBuf>,
    /// Drive images, in (channel, master/slave) order: 0-1 are channel 0's,
    /// 2-3 are channel 1's (RIPPLE only).
    pub drives: [Option<DriveImage>; 4],
}

/// `[cartridge]`: which freezer cartridge is fitted, and the image it
/// carries.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CartridgeConfig {
    /// None for no cartridge (the default).
    pub model: Option<crate::cartridge::CartridgeModel>,
    /// The cartridge image. [`BUNDLED_HRTMON_ROM`] until resolved, when
    /// a model is fitted without an image of the user's own.
    pub rom: Option<PathBuf>,
}

impl CartridgeConfig {
    pub fn fitted(&self) -> bool {
        self.model.is_some()
    }

    /// Parse a `[cartridge] model` / `--cartridge` value.
    pub fn parse_model(value: &str) -> Result<Option<crate::cartridge::CartridgeModel>> {
        match value.trim().to_ascii_lowercase().as_str() {
            "none" | "" => Ok(None),
            "hrtmon" => Ok(Some(crate::cartridge::CartridgeModel::Hrtmon)),
            other => Err(anyhow!(
                "unknown cartridge model {other:?} (expected \"none\" or \"hrtmon\")"
            )),
        }
    }

    /// The model as the config spells it.
    pub fn model_label(&self) -> &'static str {
        self.model.map_or("none", |m| m.label())
    }
}

impl LideConfig {
    /// Whether a `[lide]` section asked for a board at all: an explicit
    /// `board`, a ROM, or a drive image. `rom_bank2` alone does not count --
    /// it is validated (and rejected) as needing `rom` below, and must not
    /// silently skip that check.
    pub fn enabled(&self) -> bool {
        self.board_named || self.rom.is_some() || self.drives.iter().any(Option::is_some)
    }
}

// No Eq: `warp_boot_idle`/`warp_until` are f64 thresholds.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Emulation {
    /// Whether the machine starts running (powered on) at launch. When
    /// false, the emulator sits powered off showing a test screen until
    /// the status-bar power button is clicked -- handy for arming video
    /// capture beforehand. The power button cold-boots the machine.
    pub power_on: bool,
    /// Whether opening this configuration in the launcher runs it at once,
    /// skipping the configuration screen (default false). Read only by the
    /// launcher -- at startup for the default configuration, and by its
    /// Load... button -- never by a machine started from the command line,
    /// which was never going to show the screen anyway.
    pub auto_launch: bool,
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
    /// How fast the UI "Warp Speed" (turbo) mode runs when engaged, expressed
    /// as an output frame-skip level. See [`WarpSpeed`]. Adjustable at runtime
    /// from the Emulator menu and the keyboard.
    pub warp_speed: WarpSpeed,
    /// Warp boot: run unpaced from power-on until the boot storage
    /// (floppy/HDD LEDs) has been idle for [`Emulation::warp_boot_idle`]
    /// emulated seconds, then resume real-time pacing. Windowed sessions
    /// only (headless runs are already unpaced); the manual warp hotkey
    /// cancels it. Mutually exclusive with `warp_until` and `--run`.
    pub warp_boot: bool,
    /// The storage-idle threshold for `warp_boot`, in emulated seconds.
    pub warp_boot_idle: f64,
    /// Warp boot until an absolute emulated timestamp instead: the
    /// deterministic variant for a setup whose boot time is known.
    pub warp_until: Option<f64>,
    /// The WinUAE-compatible uaelib trap at $F0FF60 (`crate::uaelib`):
    /// guest programs toggle warp, log debug text and register resources
    /// through it. On by default; `uaelib = false` leaves $F0FF60 floating.
    pub uaelib: bool,
    /// Permit uaelib function 88's `debug_load` / `debug_save` helpers to
    /// access files below the `--run` program directory. Off by default: the
    /// trap is commonly exposed to downloaded guest programs.
    pub uaelib_files: bool,
    /// Record rewind history from power-on, so the rewind hotkey and menu item
    /// work without opening the debugger. Off by default: capturing costs a
    /// whole-machine serialize every `rewind_interval_frames` and the retained
    /// snapshots cost `rewind_budget_mb` of host memory.
    pub rewind: bool,
    /// Host-memory cap on the retained rewind snapshots. Oldest snapshots are
    /// evicted first, so this sets how far back rewind can reach: how much
    /// emulated time that buys depends on the machine's RAM size.
    pub rewind_budget_mb: usize,
    /// Emulated frames between rewind snapshots, and therefore the granularity
    /// of one rewind step. Larger is cheaper but coarser.
    pub rewind_interval_frames: u64,
    /// Run-ahead input-latency reduction: each display refresh retires
    /// `run_ahead_frames` extra emulated frames past the anchor, presents the
    /// final frame of that burst, then rewinds to the anchor boundary. Host
    /// input sampled before the burst therefore lands in guest time up to
    /// `run_ahead_frames` earlier relative to what is on screen. Costs a
    /// whole-machine snapshot plus `(run_ahead_frames + 1)`x realtime host
    /// speed. Host-coupled devices and diagnostics that cannot be rewound
    /// leave the configured value selected but temporarily inactive.
    pub run_ahead_frames: u8,
}

/// Default rewind snapshot budget in MiB when `[emulation] rewind` is on.
pub const REWIND_DEFAULT_BUDGET_MB: usize = 256;
/// Default emulated frames between rewind snapshots: half a second of PAL,
/// which is a comfortable step size for a rewind hotkey.
pub const REWIND_DEFAULT_INTERVAL_FRAMES: u64 = 25;

/// Fastest configurable run-ahead. Beyond a handful of frames the burst no
/// longer fits in one display refresh on realistic hosts, and skipping too
/// many guest animation frames reads as rubber-banding.
pub const RUN_AHEAD_MAX_FRAMES: u8 = 4;

// ---------------------------------------------------------------------------
// Autofire
//
// The `[input] autofire_hz` policy: how a held fire button is turned into a
// pulse train. It lives here rather than with the keyboard bindings in
// `keymap` because it is not a host-key concern -- the phase is a function of
// emulated time alone, and every input source (gamepad, keyboard, and any
// future one) is gated through it.
// ---------------------------------------------------------------------------

/// Autofire rates offered by the menu, in Hz. 0 is off.
pub const AUTOFIRE_RATES: [u8; 6] = [0, 3, 5, 8, 12, 16];

/// Fastest configurable autofire. Above roughly this the assert window is
/// shorter than the video frame the guest samples the port on, so the button
/// reads as noise rather than as a fast tap.
pub const AUTOFIRE_MAX_HZ: u8 = 30;

/// Label for an autofire rate.
pub fn autofire_label(hz: u8) -> String {
    if hz == 0 {
        "off".to_string()
    } else {
        format!("{hz} Hz")
    }
}

/// Whether a held fire button should be *asserted* right now, given the
/// autofire rate and how much emulated time has passed.
///
/// The phase is taken from emulated seconds rather than host frames, so the
/// rate is the same under warp, on a paced run, and on PAL or NTSC -- an
/// autofire that sped up in warp would be a different game.
pub fn autofire_asserted(hz: u8, emulated_seconds: f64) -> bool {
    if hz == 0 {
        return true; // Off: the button is simply held.
    }
    // One full press+release per 1/hz second: assert on the first half.
    let half_periods = emulated_seconds * f64::from(hz) * 2.0;
    (half_periods as i64).rem_euclid(2) == 0
}

/// Real-mode pacing budget model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PacingBudget {
    /// Debit the budget by each instruction's actual returned m68k cycle
    /// count plus the chip-bus waits it incurred, clocking the CPU at its
    /// true cycles-per-instruction. The m68k core's 68000 cycle counts are
    /// validated against the SingleStepTests corpus, so this is the correct
    /// hardware-rate model and the default. (A separate
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

/// Hard upper bound on emulated frames per presented frame in `WarpSpeed::Max`,
/// so a host that emulates faster than it presents cannot spin the event loop
/// arbitrarily long between input polls. `Max` is normally bounded first by its
/// wall-clock budget (see `WarpSpeed::time_budget_ms`); this cap only matters
/// when the host is fast enough to retire this many frames inside that budget.
pub const WARP_MAX_FRAME_CAP: usize = 1024;

/// Wall-clock budget (milliseconds) for one presented frame in `WarpSpeed::Max`.
/// The event loop emulates frames back-to-back until this much host time has
/// elapsed, then presents one frame at vsync. Kept under a 60 Hz refresh
/// interval (16.6 ms) so input is still polled and a frame still presented every
/// host refresh while the core runs flat out.
pub const WARP_MAX_BUDGET_MS: u64 = 12;

/// How fast the UI "Warp Speed" (turbo) mode runs when engaged.
///
/// Presentation is gated to the host monitor's refresh rate (the wgpu surface
/// presents with vsync), so emulating exactly one frame per presented frame
/// caps warp at the monitor rate -- about 1.2x for a 50 Hz PAL machine on a
/// 60 Hz display. To decouple emulation speed from the monitor, warp emulates
/// several frames per *presented* frame (output frame skip): the intermediate
/// frames are computed but never rendered or presented, so the effective speed
/// is the level times the refresh rate, host CPU permitting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum WarpSpeed {
    /// Two emulated frames per presented frame.
    X2,
    /// Four emulated frames per presented frame.
    X4,
    /// Eight emulated frames per presented frame.
    X8,
    /// Sixteen emulated frames per presented frame.
    X16,
    /// As many frames as fit in `WARP_MAX_BUDGET_MS` of host time per presented
    /// frame (bounded by `WARP_MAX_FRAME_CAP`): run flat out, present at vsync.
    #[default]
    Max,
}

impl WarpSpeed {
    /// Every limit, in the order a picker offers them.
    pub const MENU_ORDER: [WarpSpeed; 5] = [
        WarpSpeed::X2,
        WarpSpeed::X4,
        WarpSpeed::X8,
        WarpSpeed::X16,
        WarpSpeed::Max,
    ];

    /// Cycle to the next level for the menu/keyboard "cycle" control:
    /// 2x -> 4x -> 8x -> 16x -> Max -> 2x.
    pub fn next(self) -> Self {
        match self {
            Self::X2 => Self::X4,
            Self::X4 => Self::X8,
            Self::X8 => Self::X16,
            Self::X16 => Self::Max,
            Self::Max => Self::X2,
        }
    }

    /// Short label for menus and the on-screen status flash.
    pub fn label(self) -> &'static str {
        match self {
            Self::X2 => "2x",
            Self::X4 => "4x",
            Self::X8 => "8x",
            Self::X16 => "16x",
            Self::Max => "Max",
        }
    }

    /// Maximum emulated frames to retire per presented frame while warping.
    pub fn frame_cap(self) -> usize {
        match self {
            Self::X2 => 2,
            Self::X4 => 4,
            Self::X8 => 8,
            Self::X16 => 16,
            Self::Max => WARP_MAX_FRAME_CAP,
        }
    }

    /// Wall-clock budget (milliseconds) per presented frame, or `None` for the
    /// fixed levels, which simply retire `frame_cap` frames then present.
    pub fn time_budget_ms(self) -> Option<u64> {
        match self {
            Self::Max => Some(WARP_MAX_BUDGET_MS),
            _ => None,
        }
    }
}

/// How Paula's stereo output is presented to the host. The Amiga hardware pans
/// channels 0/3 hard left and 1/2 hard right; `Mono` averages them into both
/// output channels for listeners who dislike that hard separation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ChannelMode {
    #[default]
    Stereo,
    Mono,
}

impl ChannelMode {
    pub fn label(self) -> &'static str {
        match self {
            ChannelMode::Stereo => "stereo",
            ChannelMode::Mono => "mono",
        }
    }

    pub fn is_mono(self) -> bool {
        matches!(self, ChannelMode::Mono)
    }
}

pub(crate) fn parse_channel_mode(s: &str) -> Result<ChannelMode> {
    match s.trim().to_ascii_lowercase().as_str() {
        "stereo" => Ok(ChannelMode::Stereo),
        "mono" => Ok(ChannelMode::Mono),
        other => bail!("unknown [audio] channel_mode {other:?}; expected \"stereo\" or \"mono\""),
    }
}

/// Control over Paula's analogue low-pass ("power LED") filter. `Auto` lets the
/// guest drive it through CIA-A's /LED line, as real hardware does; `On`/`Off`
/// force it regardless of what the software asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub enum AudioFilterMode {
    #[default]
    Auto,
    On,
    Off,
}

impl AudioFilterMode {
    pub fn label(self) -> &'static str {
        match self {
            AudioFilterMode::Auto => "auto",
            AudioFilterMode::On => "on",
            AudioFilterMode::Off => "off",
        }
    }
}

pub(crate) fn parse_audio_filter_mode(s: &str) -> Result<AudioFilterMode> {
    match s.trim().to_ascii_lowercase().as_str() {
        "auto" => Ok(AudioFilterMode::Auto),
        "on" | "enabled" | "true" => Ok(AudioFilterMode::On),
        "off" | "disabled" | "false" => Ok(AudioFilterMode::Off),
        other => {
            bail!("unknown [audio] audio_filter {other:?}; expected \"auto\", \"on\", or \"off\"")
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioConfig {
    /// Synthesized floppy-drive sound effects: motor hum, head-step
    /// clacks and seek buzz (and the empty-drive change-line poll
    /// click).
    pub floppy_sounds: bool,
    /// Drive sound level, 0-100, relative to Paula's output.
    pub floppy_sounds_volume: u8,
    /// Host audio output device, matched by case-insensitive substring against
    /// the names cpal enumerates. `None` uses the system default output.
    pub output_device: Option<String>,
    /// Whether live audio output is produced at all. `false` runs with a null
    /// sink (no sound), the GUI's "Disabled" picker option; it is separate from
    /// the `--noaudio`/`--audio` CLI flags, which still override it.
    pub output_enabled: bool,
    /// Stereo (hardware panning) or mono (L/R averaged into both channels).
    pub channel_mode: ChannelMode,
    /// Stereo width, 0-100. 100 keeps the hardware left/right panning (default),
    /// 0 collapses to mono; values between narrow the separation.
    pub stereo_separation: u8,
    /// Paula's analogue low-pass filter: guest-driven (`Auto`) or forced.
    pub filter: AudioFilterMode,
    /// Default `--audio-stems-mode` granularity list, used when
    /// `--audio-stems` is given without `--audio-stems-mode` on the CLI.
    /// `None` when unset (a bare `--audio-stems` then requires an explicit
    /// `--audio-stems-mode`, per the CLI's own validation).
    pub stem_granularity: Option<Vec<crate::audio::mux::StemGranularity>>,
}

impl Default for AudioConfig {
    fn default() -> Self {
        Self {
            floppy_sounds: true,
            floppy_sounds_volume: 100,
            output_device: None,
            output_enabled: true,
            channel_mode: ChannelMode::Stereo,
            stereo_separation: 100,
            filter: AudioFilterMode::Auto,
            stem_granularity: None,
        }
    }
}

/// Where on the emulated machine a host disk is attached.
///
/// An Amiga IDE channel carries two devices, so master and slave are
/// positions on one bus rather than separate buses. A SCSI unit is a target
/// address on whichever controller the machine has fitted, which is why the
/// unit is a number here and the controller is not named: the machine has at
/// most one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum HostDiskAttach {
    #[default]
    IdeMaster,
    IdeSlave,
    /// Master on a `[lide]` Zorro II IDE board channel (0 or 1).
    LideMaster(u8),
    /// Slave on a `[lide]` Zorro II IDE board channel (0 or 1).
    LideSlave(u8),
    /// A unit on whichever SCSI controller the machine has fitted.
    Scsi(u8),
}

/// SCSI units a controller addresses. Unit 7 is the controller itself.
pub const SCSI_UNITS: u8 = 7;

impl HostDiskAttach {
    /// The spelling a configuration file uses.
    pub fn token(self) -> String {
        match self {
            Self::IdeMaster => "ide-master".to_string(),
            Self::IdeSlave => "ide-slave".to_string(),
            Self::LideMaster(ch) => format!("lide{ch}-master"),
            Self::LideSlave(ch) => format!("lide{ch}-slave"),
            Self::Scsi(unit) => format!("scsi{unit}"),
        }
    }

    /// How the launcher and logs name it.
    pub fn label(self) -> String {
        match self {
            Self::IdeMaster => "IDE Master".to_string(),
            Self::IdeSlave => "IDE Slave".to_string(),
            Self::LideMaster(ch) => format!("Lide {ch} Master"),
            Self::LideSlave(ch) => format!("Lide {ch} Slave"),
            Self::Scsi(unit) => format!("SCSI Unit {unit}"),
        }
    }

    /// What a machine with no way to attach a host disk at all is missing.
    /// Said without naming a bus: an A500 could take a SCSI card, so telling
    /// its owner "IDE needs an A600" reads as the wrong half of the answer.
    pub fn no_port_requirement() -> &'static str {
        "Host disk attach requires an A600, A1200, A4000 or SCSI controller"
    }

    /// What a machine must have for this point to exist at all. Both IDE
    /// positions share one requirement, so they share one message.
    pub fn requirement(self) -> &'static str {
        match self {
            Self::IdeMaster | Self::IdeSlave => "Attach to IDE requires an A600, A1200 or A4000",
            Self::LideMaster(_) | Self::LideSlave(_) => "Attach to Lide requires a [lide] board",
            Self::Scsi(_) => "Attach to SCSI requires a SCSI controller",
        }
    }

    /// Whether this is a SCSI unit.
    pub fn is_scsi(self) -> bool {
        matches!(self, Self::Scsi(_))
    }

    /// Every attachment point, in the order a picker cycles them.
    pub fn all() -> Vec<Self> {
        let mut all = vec![
            Self::IdeMaster,
            Self::IdeSlave,
            Self::LideMaster(0),
            Self::LideSlave(0),
            Self::LideMaster(1),
            Self::LideSlave(1),
        ];
        all.extend((0..SCSI_UNITS).map(Self::Scsi));
        all
    }

    /// Name a set of attachment points as one phrase.
    ///
    /// SCSI units collapse into a single "SCSI Unit 0,1,2": four disks on one
    /// controller are four addresses on one bus, and spelling the controller
    /// out four times says less, not more. One unit reads exactly as its own
    /// label does.
    pub fn describe_all(points: &[Self]) -> String {
        let mut parts: Vec<String> = Vec::new();
        let mut units: Vec<u8> = Vec::new();
        // The SCSI group sits where the first SCSI disk came, so the phrase
        // follows the order the disks were given.
        let mut group = None;
        for point in points {
            match point {
                Self::Scsi(unit) => {
                    if group.is_none() {
                        group = Some(parts.len());
                        parts.push(String::new());
                    }
                    units.push(*unit);
                }
                other => parts.push(other.label()),
            }
        }
        if let Some(at) = group {
            let units: Vec<String> = units.iter().map(u8::to_string).collect();
            parts[at] = format!("SCSI Unit {}", units.join(","));
        }
        parts.join(", ")
    }

    /// The point a configuration token names.
    pub fn from_token(token: &str) -> Option<Self> {
        Self::all()
            .into_iter()
            .find(|a| a.token().eq_ignore_ascii_case(token))
    }
}

/// One real host disk given to the emulated machine, from `[[host_disk]]`.
///
/// Kept apart from the image configuration deliberately. A disk is not a file:
/// it is chosen from what the host has attached, it needs the host's
/// permission to open, it may have to be taken from the host first, and the
/// operations that will grow around it -- preparing, partitioning -- have no
/// counterpart for an image. Sharing a representation with image paths would
/// mean every one of those concerns leaking into the image path.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct HostDiskConfig {
    /// The host's current enumeration name (`disk4`, `sdb`,
    /// `PhysicalDrive1`). It is a useful label and lookup fallback, not a
    /// persistent hardware identity; the `fingerprint` field provides that.
    pub device: String,
    /// Opaque hardware fingerprint captured when Copperline last saw the disk.
    /// This revalidates a persisted attachment when the host gives it a
    /// different `diskN`, `sdX`, or `PhysicalDriveN` name; weak/removable
    /// fingerprints are read-only guards, not persisted write authority.
    pub fingerprint: Option<String>,
    /// This process has just shown this exact disk to the user and received an
    /// explicit selection. Runtime-only: persisted attachments must revalidate
    /// against stable hardware identity before writable access.
    pub identity_confirmed: bool,
    /// Where the machine sees it.
    pub attach: HostDiskAttach,
    /// Whether the guest may write to the disk. Persisted and hand-written
    /// entries default to read-only; writable access must be explicit.
    pub writable: bool,
}

/// Which FluxBridge driver backs a bridged drive.
///
/// Named rather than indexed so a config file does not depend on the order the
/// installed library happens to enumerate its drivers in; the name is resolved
/// to an index when the drive is opened.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BridgeDriver {
    /// Rob Smith's Arduino-based DrawBridge.
    DrawBridge,
    /// Keir Fraser's Greaseweazle.
    #[default]
    Greaseweazle,
    /// Jim Drew's Supercard Pro.
    SupercardPro,
}

impl BridgeDriver {
    /// The library's own configuration token for this driver, which is what
    /// `fluxbridge::driver_named` resolves.
    pub fn match_token(self) -> &'static str {
        match self {
            BridgeDriver::DrawBridge => "drawbridge",
            BridgeDriver::Greaseweazle => "greaseweazle",
            BridgeDriver::SupercardPro => "supercardpro",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            BridgeDriver::DrawBridge => "DrawBridge",
            BridgeDriver::Greaseweazle => "Greaseweazle",
            BridgeDriver::SupercardPro => "Supercard Pro",
        }
    }
}

/// How hard the driver works to reproduce the disk's real timing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BridgeReadMode {
    /// Captures wherever the head happens to be, saving the wait for the index
    /// -- most of a revolution per track -- and serves the capture while the
    /// platter is still turning it, exactly as a real head does. The
    /// revolution's two ends are joined where the recording repeats; a join
    /// the library can prove, or a capture verified as a whole AmigaDOS
    /// track, replays indefinitely, and one it cannot prove is served once
    /// and re-read fresh. Reads the same disks as `Compatible` and reaches a
    /// Workbench desktop appreciably sooner.
    /// The default.
    #[default]
    Normal,
    /// Captures each track from the index, so a revolution begins where the
    /// real one does and its two ends meet in the gap between sectors, exactly
    /// as a captured image's do. Waiting for the index costs most of a
    /// revolution on every track, which is why `Normal` is the default; reach
    /// for this one if a disk reads badly without the index to anchor it.
    Compatible,
    /// As `Compatible`, but the driver holds the caller up until the track is
    /// ready instead of answering "not yet". The wait lands on the emulated
    /// machine, which stops -- pointer and all -- for as long as it takes.
    Stalling,
}

/// Whether to force a density rather than sensing it from the disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BridgeDensity {
    #[default]
    Auto,
    Dd,
    Hd,
}

/// Which drive the interface selects on the cable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BridgeCable {
    /// IBM PC cabling, drive A: the usual case for a PC drive on a
    /// Greaseweazle or DrawBridge.
    #[default]
    DriveA,
    DriveB,
    /// Shugart cabling, for a real Amiga drive.
    Shugart0,
    Shugart1,
    Shugart2,
    Shugart3,
}

/// Replay speeds a bridged bay accepts, as percentages of the platter's
/// real speed. Shared by the config parser, the CLI, and the launcher's
/// cycle row so all three offer the same set.
pub const SUPPORTED_BRIDGE_SPEED_PERCENTS: [u16; 2] = [100, 200];

/// The replay speed a bridged bay uses unless told otherwise: fast. A
/// track's first read always arrives at the platter's own pace, so this only
/// decides how re-reads are served -- and waiting out a full rotation to
/// replay bits already in hand helps nobody. `normal` is the opt-in for
/// software that times its own drive.
pub const DEFAULT_BRIDGE_SPEED_PERCENT: u16 = 200;

/// A real drive attached to one floppy bay, from `[floppy.dfN] bridge = ...`.
///
/// Held apart from [`FloppyDriveConfig`] because a bridged drive has no image
/// path: whichever of the two is present for a bay supplies its media.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FluxBridgeConfig {
    pub driver: BridgeDriver,
    /// Emulator-level write protection, on top of the disk's own tab.
    /// Defaults to true, exactly as it does for an image, so writing to a real
    /// disk takes a deliberate `write_protected = false` as well as the tab
    /// being open -- two independent things to get wrong before anything is
    /// laid on physical media.
    pub write_protected: bool,
    /// `None` auto-detects the interface, which every current driver supports.
    pub port: Option<String>,
    pub mode: BridgeReadMode,
    pub density: BridgeDensity,
    pub cable: BridgeCable,
    /// How fast replays of already-captured revolutions are served to the
    /// guest, as a percentage of the platter's real speed: `200` ("fast",
    /// the default) or `100` ("normal"). A track's first read always streams
    /// at the platter's own pace; this only compresses the wait when the
    /// guest asks for a track already in hand. As with `[floppy] speed`,
    /// software that times its own loading can notice.
    pub speed: u16,
}

impl Default for FluxBridgeConfig {
    fn default() -> Self {
        Self {
            driver: BridgeDriver::default(),
            // Protected unless told otherwise, matching both the parser and an
            // image-backed drive. A derived `Default` would make this false and
            // quietly hand out a writable real disk.
            write_protected: true,
            port: None,
            mode: BridgeReadMode::default(),
            density: BridgeDensity::default(),
            cable: BridgeCable::default(),
            speed: DEFAULT_BRIDGE_SPEED_PERCENT,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FloppyConfig {
    pub drives: [Option<FloppyDriveConfig>; 4],
    /// Real drives, by bay. A bay with a bridge here has no entry in
    /// `drives`: the physical disk is its media.
    pub bridges: [Option<FluxBridgeConfig>; 4],
    /// Emulated drive speed as a data-rate percentage: 100 (real speed),
    /// 200/400/800 (that many times faster), or 0 for turbo, where DMA
    /// transfers complete almost instantly. Values above 100 keep the full
    /// bit-level pipeline, only compressed in time; drive mechanics (motor
    /// spin-up, stepping) always run at real speed.
    pub speed: u16,
}

impl Default for FloppyConfig {
    fn default() -> Self {
        Self {
            drives: std::array::from_fn(|_| None),
            bridges: std::array::from_fn(|_| None),
            speed: 100,
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
    M68010,
    M68EC020,
    M68020,
    M68030,
    M68040,
    M68060,
}

impl CpuModel {
    /// Whether the model ships with an FPU by default: the full 68040 has
    /// its floating-point unit on-die (the FPU-less variants are the LC/EC
    /// parts, which Copperline does not model); 68881/68882 boards for the
    /// other CPUs are opt-in via `[cpu] fpu = true`.
    pub fn default_fpu(self) -> bool {
        matches!(self, CpuModel::M68040 | CpuModel::M68060)
    }

    /// Default CPU clock in MHz for this model: a stock 68000/68010 runs at
    /// the PAL system clock (~7.09 MHz, 2x the colour clock); accelerated
    /// parts default to representative speeds (020 ~14 MHz, 030/040 ~25 MHz).
    /// Fast RAM runs at the CPU clock; chip/slow RAM stays chip-bus bound.
    pub fn default_clock_mhz(self) -> f64 {
        match self {
            CpuModel::M68000 | CpuModel::M68010 => 7.09,
            CpuModel::M68EC020 | CpuModel::M68020 => 14.0,
            CpuModel::M68030 | CpuModel::M68040 => 25.0,
            CpuModel::M68060 => 50.0,
        }
    }

    /// Whether this model has the on-chip instruction cache Copperline models.
    /// The 68020/68EC020/68030 ship a 256-byte direct-mapped instruction cache
    /// and the 68040 a 4 KB one; AmigaOS enables it (CACR) at boot. Real
    /// A1200/A4000 software (demos especially) leans on it: code looping out of
    /// chip RAM otherwise contends with bitplane DMA on every fetch and runs
    /// roughly half-speed.
    pub fn has_instruction_cache(self) -> bool {
        matches!(
            self,
            CpuModel::M68EC020
                | CpuModel::M68020
                | CpuModel::M68030
                | CpuModel::M68040
                | CpuModel::M68060
        )
    }

    /// Whether this model has the on-chip data cache Copperline models. The
    /// 68030 (256 bytes) and 68040 (4 KB) have one; the 020 has none.
    pub fn has_data_cache(self) -> bool {
        matches!(self, CpuModel::M68030 | CpuModel::M68040 | CpuModel::M68060)
    }
}

/// What a 68060 does with the instructions dropped from its silicon:
/// faithful traps (the OS-side 68060.library emulates them, as on real
/// accelerator boards) or direct native execution for systems without
/// the library.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub enum UnimplementedPolicy {
    #[default]
    Trap,
    Native,
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
/// RTC presence). With no `[machine]` section the defaults match the `A500`
/// profile: the A500 Rev 6A (ECS 8372A Agnus, OCS 8362 Denise, 68000,
/// 512 KiB chip RAM, and 512 KiB trapdoor slow RAM).
///
/// Append new models at the end: savestates carry the discriminant, so
/// inserting one in the middle renames every model below it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum MachineModel {
    /// A500 Rev 6A: the ECS "Fatter" 8372A Agnus (1 MiB chip reach, software
    /// PAL/NTSC switch) with the original OCS 8362 Denise.
    A500,
    /// Early A500 (Rev 3/5), equivalent to an A2000: the 512 KiB OCS "Fat
    /// Agnus" (8370/8371) with OCS 8362 Denise.
    A500Ocs,
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
    /// A1000: the original Amiga. OCS 8361/8367 Agnus + OCS 8362 Denise, and
    /// no Kickstart ROM -- the `rom` is the 64 KiB bootstrap ROM, which loads
    /// Kickstart from the Kickstart disk in DF0 into 256 KiB of writable
    /// control store (WCS) at $FC0000 and then write-protects it. 256 KiB
    /// stock chip RAM, no trapdoor slow RAM, no RTC.
    A1000,
    /// A3000: ECS, 68030 at 25 MHz, 2 MB chip RAM, a Ramsey-04 memory
    /// controller with the stock 4 MB of motherboard fast RAM
    /// (`[memory] motherboard` resizes it), and the battery-backed Ricoh
    /// RP5C01 clock. No Gayle -- the big-box machines carry Gary -- and no
    /// slow RAM.
    A3000,
    /// A4000: the same board a generation later -- AGA, a 25 MHz 68040, and
    /// Ramsey-07 with the same stock 4 MB of motherboard fast RAM. Same
    /// story on Gayle and slow RAM as the A3000.
    A4000,
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
    /// Ramsey-controlled motherboard fast RAM (A3000/A4000).
    #[serde(default)]
    pub mb_ram_bytes: usize,
    /// CPU-slot (accelerator) fast RAM at $08000000.
    #[serde(default)]
    pub accel_ram_bytes: usize,
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
            mb_ram_bytes: 0,
            accel_ram_bytes: 0,
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
            "{profile} / {:?} / {:?} / {:?} / chip {}K fast {}K slow {}K mb {}K accel {}K / ROM {}{ext}",
            self.cpu,
            self.chipset,
            self.video_standard,
            self.chip_ram_bytes / 1024,
            self.fast_ram_bytes / 1024,
            self.slow_ram_bytes / 1024,
            self.mb_ram_bytes / 1024,
            self.accel_ram_bytes / 1024,
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
        if self.mb_ram_bytes != other.mb_ram_bytes {
            diffs.push(format!(
                "motherboard RAM {}K -> {}K",
                self.mb_ram_bytes / 1024,
                other.mb_ram_bytes / 1024
            ));
        }
        if self.accel_ram_bytes != other.accel_ram_bytes {
            diffs.push(format!(
                "accelerator RAM {}K -> {}K",
                self.accel_ram_bytes / 1024,
                other.accel_ram_bytes / 1024
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

/// Which bus gate array the machine carries. A machine has exactly one, and
/// they are not interchangeable parts so much as the same seat on the board:
/// both decode the $DE0000 page, so fitting two would make the decode
/// ambiguous.
///
/// Gayle (the wedge machines) is the bus controller plus IDE, PCMCIA, and the
/// interrupt plumbing at $DA8000-$DAA000, with an ID register at $DE1000. Fat
/// Gary (the big-box machines) is only a bus controller: three flag registers
/// on byte lanes 0-2 of the $DE0000 page, with Ramsey answering on lane 3.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum GateArray {
    #[default]
    None,
    GayleA600,
    GayleA1200,
    /// Fat Gary, as fitted to the A3000 and A4000. Always accompanied by a
    /// Ramsey (see [`MemController`]): they share one address decode.
    FatGary,
}

impl GateArray {
    /// The 8-bit ID shifted out of $DE1000 (MSB first): $D0 on the A600,
    /// $D1 on the A1200. Only Gayle has one.
    pub fn gayle_id(self) -> Option<u8> {
        match self {
            Self::None | Self::FatGary => None,
            Self::GayleA600 => Some(0xD0),
            Self::GayleA1200 => Some(0xD1),
        }
    }

    /// Whether this machine's gate array is a Fat Gary.
    pub fn is_fat_gary(self) -> bool {
        self == Self::FatGary
    }
}

/// Which memory controller the machine carries. The big-box machines put a
/// Ramsey at $DE0000, where the wedge machines put Gayle; the two are mutually
/// exclusive, and everything else has neither.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MemController {
    #[default]
    None,
    /// Ramsey-04, as fitted to the A3000.
    Ramsey4,
    /// Ramsey-07, as fitted to the A4000.
    Ramsey7,
}

impl MemController {
    pub fn ramsey_revision(self) -> Option<crate::ramsey::RamseyRevision> {
        match self {
            Self::None => None,
            Self::Ramsey4 => Some(crate::ramsey::RamseyRevision::Rev4),
            Self::Ramsey7 => Some(crate::ramsey::RamseyRevision::Rev7),
        }
    }
}

const A500_TRAPDOOR_RAM_BYTES: usize = 512 * 1024;

impl Default for Config {
    fn default() -> Self {
        Self {
            rom_path: PathBuf::from(BUNDLED_AROS_ROM),
            netplay_storage: false,
            netplay_read_only: Vec::new(),
            cpu: CpuModel::M68000,
            fpu: CpuModel::M68000.default_fpu(),
            cpu_clock_mhz: CpuModel::M68000.default_clock_mhz(),
            cpu_icache: false,
            cpu_dcache: false,
            cpu_unimplemented: UnimplementedPolicy::Trap,
            cpu_jit: false,
            emulation: Emulation {
                power_on: true,
                auto_launch: false,
                pacing_budget: PacingBudget::Cycles,
                realtime_priority: false,
                warp_speed: WarpSpeed::default(),
                warp_boot: false,
                warp_boot_idle: 10.0,
                warp_until: None,
                uaelib: true,
                uaelib_files: false,
                rewind: false,
                rewind_budget_mb: REWIND_DEFAULT_BUDGET_MB,
                rewind_interval_frames: REWIND_DEFAULT_INTERVAL_FRAMES,
                run_ahead_frames: 0,
            },
            chip_ram_bytes: 512 * 1024,
            fast_ram_bytes: 0,
            slow_ram_bytes: A500_TRAPDOOR_RAM_BYTES,
            ram_init: RamInit::Zero,
            mb_ram_bytes: 0,
            accel_ram_bytes: 0,
            z3_ram_bytes: 0,
            zorro_boards: Vec::new(),
            wasm_boards: Vec::new(),
            identify_board: true,
            filesys: Vec::new(),
            run_program_dir: None,
            host_disks: Vec::new(),
            // The no-[machine] default models the most common and most-
            // targeted Amiga: the A500 Rev 6A (the ECS "Fatter" 8372A Agnus
            // with the original OCS 8362 Denise). Selecting `[chipset]
            // revision` or a different `[machine] profile` opts out.
            chipset: Chipset::Ecs,
            agnus_revision: AgnusRevision::Ecs8372Rev4,
            denise_revision: DeniseRevision::Ocs,
            machine: None,
            gate_array: GateArray::None,
            mem_controller: MemController::None,
            rom_scsi_device_disable: false,
            log_unmapped: None,
            validate_chipset: false,
            detect_smc: false,
            ide_a4000: false,
            sdmac: false,
            akiko: false,
            cdtv_cd: false,
            extended_rom_path: None,
            fmv_rom_path: None,
            cd_image_path: None,
            cd_insert_delay_secs: 0.0,
            cd32_nvram_path: None,
            // The default machine is the A500 Rev 6A, which had no battery
            // clock; only the A500+/CDTV profiles fit one (see
            // machine_profile_defaults).
            rtc_present: false,
            rtc_chip: crate::rtc::RtcChip::Msm6242,
            rtc_seed_unix: None,
            rtc_frozen: false,
            battmem_path: None,
            video_standard: VideoStandard::Pal,
            audio: AudioConfig::default(),
            ide: IdeConfig::default(),
            scsi: ScsiConfig::default(),
            copperhf: CopperhfConfig::default(),
            lide: LideConfig::default(),
            a2065_net: None,
            toccata: false,
            cartridge: CartridgeConfig::default(),
            mhi: false,
            hostsocket_net: None,
            hostsocket_transport: None,
            rtg: RtgCard::None,
            rtg_vram_bytes: 2 * 1024 * 1024,
            floppy: FloppyConfig::default(),
            floppy_connected: [true, false, false, false],
            floppy_playlists: std::array::from_fn(|_| Vec::new()),
            overscan: Overscan::Tv,
            tv_centre: TvCentre::default(),
            pixel_aspect: PixelAspect::Tv,
            scaling: DisplayScaling::Smooth,
            autocrop: false,
            deinterlace: true,
            phosphor: 0.0,
            shader: ShaderMode::None,
            shader_strength: 1.0,
            bezel: BezelStyle::None,
            bezel_stickers: None,
            perf_overlay: false,
            tint: Tint::None,
            menu_scale: MenuScale::Normal,
            full_screen: false,
            status_bar: true,
            joystick_input_mode: JoystickInputMode::Gamepad,
            mouse_sensitivity: 50,
            mouse_capture: MouseCapture::Click,
            autofire_hz: 0,
            port_devices: [PortDevice::Mouse, PortDevice::Joystick],
            serial: SerialConfig::default(),
            parallel: ParallelConfig::default(),
            paths: crate::pathconf::Paths::default(),
        }
    }
}

impl Config {
    /// Why this resolved machine shape cannot use per-refresh run-ahead
    /// snapshots. These devices retain or mutate host state outside the
    /// serialized core. Dynamic media and observers are checked on the live
    /// Bus by the window session.
    pub fn runahead_machine_block_reason(&self) -> Option<&'static str> {
        if !self.filesys.is_empty() {
            return Some("host directory volume");
        }
        if !self.host_disks.is_empty() {
            return Some("physical host disk");
        }
        if self.ide.master.is_some()
            || self.ide.slave.is_some()
            || self.scsi.units.iter().any(Option::is_some)
            || self.copperhf.units.iter().any(Option::is_some)
            || self.lide.drives.iter().any(Option::is_some)
        {
            return Some("hard-drive or ATAPI image");
        }
        if self.a2065_net.is_some() {
            return Some("A2065 network board");
        }
        if self.hostsocket_net.is_some() || self.hostsocket_transport.is_some() {
            return Some("HostSocket network board");
        }
        if !self.wasm_boards.is_empty() {
            return Some("WASM expansion board");
        }
        if self.mhi {
            return Some("MHI decoder board");
        }
        None
    }

    /// Load a config, applying command-line overrides on top of whatever the
    /// file (or the built-in defaults, when `path` is `None`) provides. The
    /// overrides are injected into the raw TOML view before validation, so
    /// they go through exactly the same profile-defaulting, derivation, and
    /// range-checking as the equivalent config fields would.
    /// The raw TOML view a config is loaded from, with the CLI overrides
    /// already applied but before validation/derivation. `main` validates this
    /// into a [`Config`] to build the machine and also keeps the raw view, so
    /// the configuration screen can reopen showing the running machine's
    /// settings and re-emit them on Save.
    pub fn load_raw(path: Option<&Path>, overrides: &ConfigOverrides) -> Result<RawConfig> {
        let mut raw = match path {
            Some(p) => raw_from_path(p)?,
            None => RawConfig::default(),
        };
        overrides.apply_to(&mut raw);
        Ok(raw)
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
            mb_ram_bytes: self.mb_ram_bytes,
            accel_ram_bytes: self.accel_ram_bytes,
            chipset: self.chipset,
            video_standard: self.video_standard,
            machine: self.machine,
            rom: RomId::default(),
            extended_rom: None,
        }
    }

    /// Build the Zorro autoconfig chain this config asks for: the built-in
    /// Zorro II fast RAM board, the built-in Zorro III RAM board, any
    /// `[[zorro]]` metadata boards in file order, and finally (unless
    /// `identify = false`) the Copperline identification board. The ID board
    /// comes last so the configured RAM boards keep the autoconfig base
    /// addresses they would get without it.
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
        if self.identify_board {
            chain.add_board(BoardSpec::copperline_id())?;
        }
        // The Copperline services board itself (`[[filesys]]`) is a
        // functional device, added in emulator.rs where its device slot is
        // assigned (like the A4091); only the config validation lives here.
        if !self.filesys.is_empty() || self.rom_scsi_device_disable {
            if self.filesys.len() > crate::filesys::MOUNT_MAX_COUNT {
                anyhow::bail!(
                    "[[filesys]]: at most {} mounts supported",
                    crate::filesys::MOUNT_MAX_COUNT
                );
            }
            for m in &self.filesys {
                if !m.path.is_dir() {
                    anyhow::bail!("[[filesys]] path {} is not a directory", m.path.display());
                }
                if let Some(err) = crate::filesys::volume_name_error(&m.volume) {
                    anyhow::bail!("[[filesys]] {err}");
                }
            }
        }
        Ok(chain)
    }
}
/// Command-line overrides for the handful of machine knobs it is convenient
/// to set without writing a config file: the machine model, the chipset
/// preset, the CPU and its FPU/clock, and the chip/fast/slow RAM sizes. Each
/// field is `None` when the corresponding flag was not given, leaving the file
/// (or profile default) value untouched. The string fields carry the same
/// syntax the matching TOML fields accept and are validated by the same
/// parsers.
#[derive(Debug, Default, Clone)]
pub struct ConfigOverrides {
    pub model: Option<String>,
    pub chipset: Option<String>,
    /// PAL/NTSC video standard (`--video`).
    pub video: Option<String>,
    pub cpu: Option<String>,
    pub fpu: Option<bool>,
    pub cpu_clock_mhz: Option<f64>,
    /// Fast CPU execution via the batch/trace-JIT path (`--jit`/`--no-jit`).
    /// Same semantics as `[cpu] jit`.
    pub cpu_jit: Option<bool>,
    pub chip: Option<String>,
    pub fast: Option<String>,
    pub slow: Option<String>,
    /// RAM power-on policy (`--ram-init`). Same syntax as `[memory] init`.
    pub ram_init: Option<String>,
    /// Ramsey motherboard fast RAM size (`--motherboard`). Same parser as
    /// `[memory] motherboard`.
    pub motherboard: Option<String>,
    /// CPU-slot accelerator fast RAM size (`--accelerator`). Same parser as
    /// `[memory] accelerator`.
    pub accelerator: Option<String>,
    pub floppy_drives: Option<u8>,
    /// Drive speed override (`--floppy-speed`): a percentage (100/200/400/
    /// 800) or 0 for turbo. Same values as `[floppy] speed`.
    pub floppy_speed: Option<u16>,
    /// Initial joystick input mode (`--joystick`): "gamepad" or "keyboard"
    /// ("auto" still accepted as a compatibility alias). Validated by the same
    /// parser as `[input] joystick`.
    pub joystick: Option<String>,
    /// Host mouse sensitivity (`--mouse-sensitivity`), 0-100. Same as
    /// `[input] mouse_sensitivity`.
    pub mouse_sensitivity: Option<u16>,
    /// When the host mouse is grabbed (`--mouse-capture`): "click",
    /// "auto", or "manual". Same parser as `[input] mouse_capture`.
    pub mouse_capture: Option<String>,
    /// Device in game port 1 (`--port1`): "mouse", "joystick", "cd32",
    /// "analogue", or "none". Same parser as `[input] port1`.
    pub port1: Option<String>,
    /// Device in game port 2 (`--port2`). Same parser as `[input] port2`.
    pub port2: Option<String>,
    /// Autofire rate in Hz (`--autofire`), 0 for off. Same validation as
    /// `[input] autofire_hz`.
    pub autofire_hz: Option<u8>,
    /// Run-ahead frames (`--run-ahead`), 0 for off. Same validation as
    /// `[emulation] run_ahead_frames`.
    pub run_ahead_frames: Option<u8>,
    /// Serial port wiring (`--serial`): "off", "stdout", "midi", "tcp",
    /// "tcp-connect", or "pty" ("none" and "terminal" parse as
    /// compatibility aliases of the first two). Same parser as
    /// `[serial] mode`.
    pub serial: Option<String>,
    /// Remote host:port the serial port dials (`--serial-connect`),
    /// implying `--serial tcp-connect`.
    pub serial_connect: Option<String>,
    /// A scripted session file to replay (`--serial-session`), implying
    /// `--serial modem`. Same as `[serial] session`.
    pub serial_session: Option<String>,
    /// Host MIDI output endpoint (`--midi-out`), implying `--serial midi`.
    pub midi_out: Option<String>,
    /// Host MIDI input endpoint (`--midi-in`), implying `--serial midi`.
    pub midi_in: Option<String>,
    /// Parallel port device (`--parallel`): "none", "printer", or "sampler".
    /// Same parser as `[parallel] device`.
    pub parallel: Option<String>,
    /// Sampler host capture device (`--sampler-audio-input`), implying
    /// `--parallel sampler`. Substring match.
    pub sampler_input: Option<String>,
    /// Sampler input gain in decibels (`--sampler-input-gain`), implying
    /// `--parallel sampler`. Preamp; 0 dB = unity.
    pub sampler_gain: Option<f32>,
    /// Host audio output device (`--audio-device`), substring match.
    pub audio_device: Option<String>,
    /// Output channel mode (`--audio-channel-mode`): "stereo" or "mono".
    pub audio_channel_mode: Option<String>,
    /// Paula audio filter mode (`--audio-filter`): "auto", "on", or "off".
    pub audio_filter: Option<String>,
    /// Stereo separation percent (`--audio-stereo-separation`), 0-100.
    pub audio_stereo_separation: Option<u16>,
    /// Power-on RTC value (`--rtc-time`): Unix seconds or
    /// "YYYY-MM-DD HH:MM[:SS]". Same parser as `[machine] rtc_time`.
    pub rtc_time: Option<String>,
    /// Freeze the seeded RTC (`--rtc-frozen`). Same as
    /// `[machine] rtc_frozen`.
    pub rtc_frozen: Option<bool>,
    /// A2065 Ethernet backend (`--a2065-net`): "none", "loopback", "nat", or
    /// "bridge".
    /// Same parser as `[a2065] net`; setting it fits the board.
    pub a2065_net: Option<String>,
    /// Host adapter for bridged A2065 networking (`--a2065-interface`).
    pub a2065_interface: Option<String>,
    /// HostSocket bsdsocket.library backend (`--hostsocket-net`): "none",
    /// "loopback", "nat", "bridge", or "host" (the Amiberry-style
    /// host-socket backend -- see `[hostsocket] net`'s own doc comment).
    /// Same parser as `[hostsocket] net`; setting it fits the board.
    pub hostsocket_net: Option<String>,
    /// Host adapter for bridged HostSocket networking
    /// (`--hostsocket-interface`).
    pub hostsocket_interface: Option<String>,
    /// Open fullscreen at start (`--full-screen` / `--windowed`). Same as
    /// `[display] full_screen`.
    pub full_screen: Option<bool>,
    /// Show the status bar at start (`--show-status-bar` /
    /// `--hide-status-bar`). Same as `[display] status_bar`.
    pub status_bar: Option<bool>,
    /// Show the performance overlay at start (`--perf-overlay`). Same as
    /// `[display] perf_overlay`.
    pub perf_overlay: Option<bool>,
    /// How large the pop-up menu is drawn (`--menu-scale`). Same values as
    /// `[display] menu_scale`.
    pub menu_scale: Option<String>,
    /// MT-32 control and PCM ROM images (`--mt32-control-rom`,
    /// `--mt32-pcm-rom`). Same as `[serial] mt32_control_rom`/`mt32_pcm_rom`.
    pub mt32_control_rom: Option<String>,
    pub mt32_pcm_rom: Option<String>,
    /// Show the MT-32's front panel (`--mt32-panel`). Same as
    /// `[serial] mt32_panel`.
    pub mt32_panel: Option<bool>,
    /// Freezer cartridge (`--cartridge MODEL`): "none" or "hrtmon". Same
    /// parser as `[cartridge] model`.
    pub cartridge: Option<String>,
    /// Real host disks given to the machine (`--host-disk DEVICE [ATTACH]`, or
    /// `--host-disk-read-only` for the protected form), in command-line order.
    pub host_disks: Vec<HostDiskArg>,
    /// A real floppy drive on a bay (`--floppy-bridge DFN INTERFACE`), by bay.
    /// Same values as `[floppy.dfN] bridge`.
    pub floppy_bridge: [Option<String>; 4],
    /// The interface's serial port (`--floppy-bridge-port DFN PORT`). Same as
    /// `[floppy.dfN] bridge_port`; unset auto-detects.
    pub floppy_bridge_port: [Option<String>; 4],
    /// Which drive on the cable to select (`--floppy-bridge-cable DFN CABLE`).
    /// Same as `[floppy.dfN] bridge_cable`.
    pub floppy_bridge_cable: [Option<String>; 4],
    /// Drop the emulator's own write protection on a bay
    /// (`--floppy-bridge-writable DFN`), leaving only the disk's tab between
    /// the guest and the platter. Same as `[floppy.dfN] write_protected =
    /// false`; there is deliberately no flag the other way, because protected
    /// is already the default.
    pub floppy_bridge_writable: [bool; 4],
    /// How the interface captures a track (`--floppy-bridge-mode DFN MODE`).
    /// Same as `[floppy.dfN] bridge_mode`.
    pub floppy_bridge_mode: [Option<String>; 4],
    /// Force a density rather than sensing it (`--floppy-bridge-density DFN
    /// DENSITY`). Same as `[floppy.dfN] bridge_density`.
    pub floppy_bridge_density: [Option<String>; 4],
    /// Serve captured tracks at a percentage of real speed
    /// (`--floppy-bridge-speed DFN PERCENT`). Same as `[floppy.dfN]
    /// bridge_speed`.
    pub floppy_bridge_speed: [Option<u16>; 4],
}

impl ConfigOverrides {
    /// Whether any override was set.
    pub fn is_empty(&self) -> bool {
        self.model.is_none()
            && self.chipset.is_none()
            && self.video.is_none()
            && self.cpu.is_none()
            && self.fpu.is_none()
            && self.cpu_clock_mhz.is_none()
            && self.cpu_jit.is_none()
            && self.chip.is_none()
            && self.fast.is_none()
            && self.slow.is_none()
            && self.ram_init.is_none()
            && self.motherboard.is_none()
            && self.accelerator.is_none()
            && self.floppy_drives.is_none()
            && self.floppy_speed.is_none()
            && self.floppy_bridge.iter().all(Option::is_none)
            && self.floppy_bridge_port.iter().all(Option::is_none)
            && self.floppy_bridge_cable.iter().all(Option::is_none)
            && self.floppy_bridge_mode.iter().all(Option::is_none)
            && self.floppy_bridge_density.iter().all(Option::is_none)
            && self.floppy_bridge_speed.iter().all(Option::is_none)
            && !self.floppy_bridge_writable.iter().any(|w| *w)
            && self.host_disks.is_empty()
            && self.joystick.is_none()
            && self.mouse_sensitivity.is_none()
            && self.mouse_capture.is_none()
            && self.port1.is_none()
            && self.port2.is_none()
            && self.autofire_hz.is_none()
            && self.run_ahead_frames.is_none()
            && self.serial.is_none()
            && self.serial_connect.is_none()
            && self.serial_session.is_none()
            && self.midi_out.is_none()
            && self.midi_in.is_none()
            && self.parallel.is_none()
            && self.sampler_input.is_none()
            && self.sampler_gain.is_none()
            && self.audio_device.is_none()
            && self.audio_channel_mode.is_none()
            && self.audio_filter.is_none()
            && self.audio_stereo_separation.is_none()
            && self.rtc_time.is_none()
            && self.rtc_frozen.is_none()
            && self.a2065_net.is_none()
            && self.a2065_interface.is_none()
            && self.hostsocket_net.is_none()
            && self.hostsocket_interface.is_none()
            && self.full_screen.is_none()
            && self.status_bar.is_none()
            && self.perf_overlay.is_none()
            && self.menu_scale.is_none()
            && self.mt32_control_rom.is_none()
            && self.mt32_pcm_rom.is_none()
            && self.mt32_panel.is_none()
            && self.cartridge.is_none()
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
        if let Some(video) = &self.video {
            raw.chipset.video = Some(video.clone());
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
        if let Some(jit) = self.cpu_jit {
            raw.cpu.jit = Some(jit);
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
        if let Some(init) = &self.ram_init {
            raw.memory.init = Some(init.clone());
        }
        if let Some(motherboard) = &self.motherboard {
            raw.memory.motherboard = Some(motherboard.clone());
        }
        if let Some(accelerator) = &self.accelerator {
            raw.memory.accelerator = Some(accelerator.clone());
        }
        if let Some(drives) = self.floppy_drives {
            raw.floppy.drives = Some(drives);
        }
        if let Some(speed) = self.floppy_speed {
            raw.floppy.speed = Some(speed);
        }
        // Real host disks named on the command line are added to whatever the
        // file already asked for; the parser is what refuses two disks, or a
        // disk and an image, on one attachment point.
        raw.host_disk
            .extend(self.host_disks.iter().map(|disk| RawHostDisk {
                device: disk.device.clone(),
                fingerprint: None,
                identity_confirmed: true,
                attach: disk.attach.clone(),
                // A command-line disk is an explicit choice on this run. Both
                // flags write the access mode down so the safe config default
                // cannot silently turn --host-disk into read-only.
                read_only: Some(disk.read_only),
            }));
        for idx in 0..4 {
            if self.floppy_bridge[idx].is_none()
                && self.floppy_bridge_port[idx].is_none()
                && self.floppy_bridge_cable[idx].is_none()
                && !self.floppy_bridge_writable[idx]
                && self.floppy_bridge_mode[idx].is_none()
                && self.floppy_bridge_density[idx].is_none()
                && self.floppy_bridge_speed[idx].is_none()
            {
                continue;
            }
            // A bay named on the command line gets a table if it had none, so
            // a bridge can be asked for with no config file at all.
            let drive = match idx {
                0 => &mut raw.floppy.df0,
                1 => &mut raw.floppy.df1,
                2 => &mut raw.floppy.df2,
                _ => &mut raw.floppy.df3,
            }
            .get_or_insert_with(RawFloppyDrive::default);
            if let Some(bridge) = &self.floppy_bridge[idx] {
                drive.bridge = Some(bridge.clone());
                // The flag says "this bay is a real drive", so an image the
                // config file left here would otherwise be a contradiction the
                // parser rejects. The command line wins, as it does elsewhere.
                // Except for `off`, whose whole point is to hand the bay back
                // to images: clearing the path there would take away the very
                // disk the config file asked for.
                if !bridge.trim().eq_ignore_ascii_case("off") {
                    drive.path = None;
                    drive.paths = None;
                }
            }
            if let Some(port) = &self.floppy_bridge_port[idx] {
                drive.bridge_port = Some(port.clone());
            }
            if let Some(cable) = &self.floppy_bridge_cable[idx] {
                drive.bridge_cable = Some(cable.clone());
            }
            if self.floppy_bridge_writable[idx] {
                drive.write_protected = Some(false);
            }
            if let Some(mode) = &self.floppy_bridge_mode[idx] {
                drive.bridge_mode = Some(mode.clone());
            }
            if let Some(density) = &self.floppy_bridge_density[idx] {
                drive.bridge_density = Some(density.clone());
            }
            if let Some(speed) = self.floppy_bridge_speed[idx] {
                drive.bridge_speed = Some(RawReplaySpeed::Percent(speed));
            }
        }
        if let Some(joystick) = &self.joystick {
            raw.input.joystick = Some(joystick.clone());
        }
        if let Some(sensitivity) = self.mouse_sensitivity {
            raw.input.mouse_sensitivity = Some(sensitivity);
        }
        if let Some(capture) = &self.mouse_capture {
            raw.input.mouse_capture = Some(capture.clone());
        }
        if let Some(port1) = &self.port1 {
            raw.input.port1 = Some(port1.clone());
        }
        if let Some(port2) = &self.port2 {
            raw.input.port2 = Some(port2.clone());
        }
        if let Some(hz) = self.autofire_hz {
            raw.input.autofire_hz = Some(hz);
        }
        if let Some(frames) = self.run_ahead_frames {
            raw.emulation.run_ahead_frames = Some(frames);
        }
        if let Some(mode) = &self.serial {
            raw.serial.mode = Some(mode.clone());
        }
        if let Some(addr) = &self.serial_connect {
            raw.serial.connect = Some(addr.clone());
        }
        if let Some(path) = &self.serial_session {
            raw.serial.session = Some(path.clone());
        }
        if let Some(out) = &self.midi_out {
            raw.serial.midi_out = Some(out.clone());
        }
        if let Some(input) = &self.midi_in {
            raw.serial.midi_in = Some(input.clone());
        }
        // Naming a MIDI endpoint or a dial-out address on the command line
        // selects the matching mode unless `--serial` said otherwise.
        if self.serial.is_none() && (self.midi_out.is_some() || self.midi_in.is_some()) {
            raw.serial.mode = Some(SerialMode::Midi.label().to_string());
        }
        if self.serial.is_none()
            && self.midi_out.is_none()
            && self.midi_in.is_none()
            && self.serial_connect.is_some()
        {
            raw.serial.mode = Some(SerialMode::TcpConnect.label().to_string());
        }
        if self.serial.is_none()
            && self.midi_out.is_none()
            && self.midi_in.is_none()
            && self.serial_connect.is_none()
            && self.serial_session.is_some()
        {
            raw.serial.mode = Some(SerialMode::Modem.label().to_string());
        }
        if let Some(device) = &self.parallel {
            raw.parallel.device = Some(device.clone());
        }
        if let Some(input) = &self.sampler_input {
            raw.parallel.sampler_input = Some(input.clone());
        }
        if let Some(gain) = self.sampler_gain {
            raw.parallel.sampler_gain = Some(gain);
        }
        // Naming a sampler option selects the sampler unless `--parallel` said
        // otherwise (mirrors `--midi-out` implying `--serial midi`).
        if self.parallel.is_none() && (self.sampler_input.is_some() || self.sampler_gain.is_some())
        {
            raw.parallel.device = Some(ParallelDevice::Sampler.label().to_string());
        }
        if let Some(dev) = &self.audio_device {
            raw.audio.output_device = Some(dev.clone());
        }
        if let Some(mode) = &self.audio_channel_mode {
            raw.audio.channel_mode = Some(mode.clone());
        }
        if let Some(filter) = &self.audio_filter {
            raw.audio.audio_filter = Some(filter.clone());
        }
        if let Some(sep) = self.audio_stereo_separation {
            raw.audio.stereo_separation = Some(sep);
        }
        if let Some(time) = &self.rtc_time {
            // The text form parses bare digits as Unix seconds, so both
            // CLI notations funnel through one raw variant.
            raw.machine.rtc_time = Some(RawRtcTime::Text(time.clone()));
        }
        if let Some(frozen) = self.rtc_frozen {
            raw.machine.rtc_frozen = Some(frozen);
        }
        if let Some(net) = &self.a2065_net {
            raw.a2065.net = Some(net.clone());
            if !matches!(
                net.trim().to_ascii_lowercase().as_str(),
                "bridge" | "bridged"
            ) {
                raw.a2065.interface = None;
            }
        }
        if let Some(interface) = &self.a2065_interface {
            raw.a2065.interface = Some(interface.clone());
            if self.a2065_net.is_none() {
                raw.a2065.net = Some("bridge".to_string());
            }
        }
        if let Some(net) = &self.hostsocket_net {
            raw.hostsocket.net = Some(net.clone());
            if !matches!(
                net.trim().to_ascii_lowercase().as_str(),
                "bridge" | "bridged"
            ) {
                raw.hostsocket.interface = None;
            }
        }
        if let Some(interface) = &self.hostsocket_interface {
            raw.hostsocket.interface = Some(interface.clone());
            if self.hostsocket_net.is_none() {
                raw.hostsocket.net = Some("bridge".to_string());
            }
        }
        if let Some(full_screen) = self.full_screen {
            raw.display.full_screen = Some(full_screen);
        }
        if let Some(status_bar) = self.status_bar {
            raw.display.status_bar = Some(status_bar);
        }
        if let Some(perf_overlay) = self.perf_overlay {
            raw.display.perf_overlay = Some(perf_overlay);
        }
        if let Some(menu_scale) = &self.menu_scale {
            raw.display.menu_scale = Some(menu_scale.clone());
        }
        if let Some(rom) = &self.mt32_control_rom {
            raw.serial.mt32_control_rom = Some(rom.clone());
        }
        if let Some(rom) = &self.mt32_pcm_rom {
            raw.serial.mt32_pcm_rom = Some(rom.clone());
        }
        if let Some(panel) = self.mt32_panel {
            raw.serial.mt32_panel = Some(panel);
        }
        if let Some(model) = &self.cartridge {
            raw.cartridge.model = Some(model.clone());
            // `--cartridge none` unfits the cartridge outright, so an image
            // the config file named goes with it: left in place, validation
            // would reject the rom as having no cartridge to apply to. Any
            // other value (a model, or a misspelling) keeps the file's image
            // for validation to pair with the model or report.
            if matches!(CartridgeConfig::parse_model(model), Ok(None)) {
                raw.cartridge.rom = None;
            }
        }
    }
}
