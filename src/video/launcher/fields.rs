// SPDX-License-Identifier: GPL-3.0-or-later

//! Launcher tabs, row definitions and choice lists.

use super::*;

/// The configuration screen's category tabs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LauncherTab {
    System,
    Cpu,
    Memory,
    Rom,
    Floppy,
    Storage,
    /// FluxBridge settings for one bay, reached from its Configure button.
    FluxBridge,
    BootPriority,
    /// The rest of the boot order when one page cannot hold it: page `N`
    /// (`N` >= 2) of the list, reached by the "Next Page >" button and
    /// returning to the first page via its own Back. A fully fitted machine
    /// reaches twenty drives across four board families, more than one page
    /// holds, so however many pages the row count needs are offered --
    /// [`MachineSetup::boot_priority_page_count`] says how many there are.
    BootPriorityMore(u8),
    HostFs,
    /// Direct WHDLoad boot (src/whdload.rs): the game to launch and what
    /// staging draws on, reached from the Storage tab.
    Whdload,
    /// The games found beside the one chosen to launch, with what the game
    /// database says about them. Only in a build with the library.
    #[cfg(feature = "game-library")]
    WhdloadLibrary,
    /// Real host storage -- an SD card, a CF card, an Amiga's own hard
    /// drive -- attached in place of a disk image. Drawn as its own layout
    /// rather than a list of settings rows, because choosing a disk is a
    /// table with a single selection, not a field.
    HostDisk,
    Cd,
    /// The `[lide]` built-in Zorro II IDE board: personality, boot ROM(s),
    /// and its drives, reached from the Storage tab.
    Lide,
    /// Copperline's own virtual hardfile controller (`[copperhf]`,
    /// copperhf.device): seven units, no board/ROM to choose -- the board
    /// is always there. Reached from the Storage tab, like Lide.
    Copperhf,
    /// The "I/O Ports" strip tab, whose default category is the serial
    /// port. Parallel, networking and audio are its sibling categories,
    /// switched between via the top nav row, with no Back button --
    /// `IoPorts.label()` is therefore the strip's "I/O Ports", not
    /// "Serial Port".
    IoPorts,
    IoParallel,
    IoNetworking,
    IoAudio,
    Input,
    Netplay,
    Zorro,
    /// The "A/V & Emu" strip tab, whose default category is Audio (its rows are
    /// the audio settings). Video and Emulation are its sibling categories,
    /// switched between via the top nav row, with no Back button.
    /// `AvAudio.label()` is therefore the strip's "A/V & Emu", not "Audio".
    AvAudio,
    AvVideo,
    /// The host window and its furniture -- fullscreen at start, the status
    /// bar, the perf overlay, menu size -- as distinct from `AvVideo`, which
    /// is the emulated picture itself.
    AvDisplay,
    AvEmulation,
    /// Where Copperline keeps what it produces and where its file dialogs
    /// open. Its rows are the configuration's `[paths]` section, saved
    /// with the rest of it and absent until one of them is set.
    AvPaths,
    /// The Create Image workshop, reached from Storage: two pages that make
    /// fresh images and touch nothing about the machine.
    CreateFloppy,
    CreateHard,
    /// The hard-disk page's geometry editor, reached from its Configure
    /// button once the geometry is set by hand.
    CreateGeometry,
}

/// Tabs shown top to bottom.
pub const TABS: &[LauncherTab] = &[
    LauncherTab::System,
    LauncherTab::Cpu,
    LauncherTab::Memory,
    LauncherTab::Rom,
    LauncherTab::Floppy,
    LauncherTab::Storage,
    // Cd, HostFs, and BootPriority are reached as sub-pages from the Storage
    // tab, so they are not top-level strip entries.
    LauncherTab::Input,
    LauncherTab::Netplay,
    LauncherTab::IoPorts,
    LauncherTab::Zorro,
    LauncherTab::AvAudio,
];

/// The strip with WHDLoad in it, between Zorro and A/V & Emu. This is the
/// usual one: the entry is there unless somebody has turned WHDLoad off.
#[cfg(feature = "game-library")]
pub(super) const WHDLOAD_TABS: &[LauncherTab] = &[
    LauncherTab::System,
    LauncherTab::Cpu,
    LauncherTab::Memory,
    LauncherTab::Rom,
    LauncherTab::Floppy,
    LauncherTab::Storage,
    LauncherTab::Input,
    LauncherTab::Netplay,
    LauncherTab::IoPorts,
    LauncherTab::Zorro,
    LauncherTab::WhdloadLibrary,
    LauncherTab::AvAudio,
];

/// The left-hand strip. WHDLoad has an entry of its own unless it has been
/// turned off in A/V & Emu -> Emulation.
///
/// It lands on the Library rather than on the settings behind it: picking
/// a game is the reason to go there, and the settings are one click away
/// on the page itself.
pub fn tabs(whdload_enabled: bool) -> &'static [LauncherTab] {
    #[cfg(feature = "game-library")]
    if whdload_enabled {
        return WHDLOAD_TABS;
    }
    let _ = whdload_enabled;
    TABS
}

impl LauncherTab {
    pub fn label(self) -> &'static str {
        match self {
            LauncherTab::System => "System",
            LauncherTab::Cpu => "CPU",
            LauncherTab::Memory => "Memory",
            LauncherTab::Rom => "ROM",
            LauncherTab::Floppy => "Floppy",
            LauncherTab::FluxBridge => "FluxBridge",
            LauncherTab::Storage => "Storage",
            LauncherTab::BootPriority | LauncherTab::BootPriorityMore(_) => "Boot Priority",
            LauncherTab::HostFs => "Host Folder",
            LauncherTab::Whdload => "WHDLoad",
            // The strip's own name for it. Inside the WHDLoad pages the
            // nav chips say Settings... and Library, from their own
            // labels, so this one is free to say which tab it is.
            #[cfg(feature = "game-library")]
            LauncherTab::WhdloadLibrary => "WHDLoad",
            LauncherTab::HostDisk => "Host Disk",
            LauncherTab::Cd => "CD",
            LauncherTab::Lide => "Lide",
            LauncherTab::Copperhf => "Copperline HD",
            LauncherTab::IoPorts => "I/O Ports",
            LauncherTab::IoParallel => "Parallel Port",
            LauncherTab::IoNetworking => "Networking",
            LauncherTab::IoAudio => "Audio",
            LauncherTab::Input => "Input",
            LauncherTab::Netplay => "Netplay",
            LauncherTab::Zorro => "Zorro",
            LauncherTab::AvAudio => "A/V & Emu",
            LauncherTab::AvVideo => "Video",
            LauncherTab::AvDisplay => "Display",
            LauncherTab::AvEmulation => "Emulation",
            LauncherTab::AvPaths => "Paths",
            LauncherTab::CreateFloppy => "Floppy Disk",
            LauncherTab::CreateHard => "Hard Disk",
            LauncherTab::CreateGeometry => "Disk Geometry",
        }
    }

    /// The strip entry to highlight for this (possibly sub-page) tab: the Storage
    /// sub-pages keep the Storage strip entry lit, and the A/V categories keep
    /// the A/V & Emu one.
    pub fn strip_tab(self) -> LauncherTab {
        match self {
            // The settings page lights the Library entry: they are two
            // views of one thing, reached through one strip entry.
            #[cfg(feature = "game-library")]
            LauncherTab::Whdload => LauncherTab::WhdloadLibrary,
            #[cfg(not(feature = "game-library"))]
            LauncherTab::Whdload => LauncherTab::Storage,
            LauncherTab::Cd
            | LauncherTab::HostFs
            | LauncherTab::HostDisk
            | LauncherTab::BootPriority
            | LauncherTab::BootPriorityMore(_)
            | LauncherTab::Lide
            | LauncherTab::Copperhf
            | LauncherTab::CreateFloppy
            | LauncherTab::CreateHard
            | LauncherTab::CreateGeometry => LauncherTab::Storage,
            LauncherTab::FluxBridge => LauncherTab::Floppy,
            LauncherTab::AvVideo
            | LauncherTab::AvDisplay
            | LauncherTab::AvEmulation
            | LauncherTab::AvPaths => LauncherTab::AvAudio,
            LauncherTab::IoParallel | LauncherTab::IoNetworking | LauncherTab::IoAudio => {
                LauncherTab::IoPorts
            }
            other => other,
        }
    }

    /// The parent tab a sub-page returns to via its Back button, or `None` when
    /// the page has no Back (the A/V categories switch between each other via the
    /// top nav row instead).
    pub fn parent_tab(self) -> Option<LauncherTab> {
        match self {
            // With the library, WHDLoad is a strip entry of its own and its
            // two pages switch between each other on the nav row, so there
            // is nowhere for Back to go. Without it, the one page is still
            // a Storage sub-page.
            #[cfg(not(feature = "game-library"))]
            LauncherTab::Whdload => Some(LauncherTab::Storage),
            LauncherTab::Cd
            | LauncherTab::HostFs
            | LauncherTab::HostDisk
            | LauncherTab::BootPriority
            | LauncherTab::Lide
            | LauncherTab::Copperhf
            | LauncherTab::CreateFloppy
            | LauncherTab::CreateHard => Some(LauncherTab::Storage),
            // Back goes to the page that sent you here, not to Storage.
            LauncherTab::CreateGeometry => Some(LauncherTab::CreateHard),
            // Every later page's Back goes straight to the first rather than
            // stepping back one page at a time -- the same "one fixed
            // parent" shape every other sub-page's Back button has.
            LauncherTab::BootPriorityMore(_) => Some(LauncherTab::BootPriority),
            LauncherTab::FluxBridge => Some(LauncherTab::Floppy),
            _ => None,
        }
    }

    /// The top nav row's buttons -- sibling pages reachable from here as
    /// `(label, tab)` pairs. Empty when the page shows a Back button instead.
    /// The button whose tab is the current page is drawn highlighted.
    pub fn nav_options(self) -> &'static [(&'static str, LauncherTab)] {
        match self {
            LauncherTab::Storage => STORAGE_NAV,
            LauncherTab::AvAudio
            | LauncherTab::AvVideo
            | LauncherTab::AvDisplay
            | LauncherTab::AvEmulation
            | LauncherTab::AvPaths => AV_NAV,
            LauncherTab::IoPorts
            | LauncherTab::IoParallel
            | LauncherTab::IoNetworking
            | LauncherTab::IoAudio => IO_NAV,
            LauncherTab::CreateFloppy | LauncherTab::CreateHard => CREATE_NAV,
            #[cfg(feature = "game-library")]
            LauncherTab::Whdload | LauncherTab::WhdloadLibrary => WHDLOAD_NAV,
            _ => &[],
        }
    }

    /// Whether this tab shows the nav row (its sibling-page links or a Back
    /// button) at the top of the pane, above its settings.
    pub fn has_top_nav(self) -> bool {
        !self.nav_options().is_empty() || self.parent_tab().is_some()
    }
}

/// The Storage tab's top nav links (its sub-pages), left to right.
pub(super) const STORAGE_NAV: &[(&str, LauncherTab)] = &[
    // The first row is the hardware -- what a machine can have attached.
    ("CD", LauncherTab::Cd),
    ("Host Folder", LauncherTab::HostFs),
    ("Host Disk", LauncherTab::HostDisk),
    ("Lide", LauncherTab::Lide),
    ("Copperline HD", LauncherTab::Copperhf),
    // Four to a row, so copperhf wraps onto the second alongside what is
    // done with the hardware above: the boot order across everything, and
    // the one entry that makes something rather than attaching something.
    ("Boot Priority", LauncherTab::BootPriority),
    ("Create Image...", LauncherTab::CreateFloppy),
];

/// The workshop's two pages. Reached from Storage, so they show a Back
/// button *and* this nav: one says where you came from, the other which of
/// the two you are on.
/// WHDLoad's two pages: what a package boots with, and which package.
/// Only a build with the library has the second, so only that one splits.
#[cfg(feature = "game-library")]
pub(super) const WHDLOAD_NAV: &[(&str, LauncherTab)] = &[
    // The library first: it is what the strip entry opens on, and what
    // somebody is there for. The settings behind it are one click away.
    ("Library", LauncherTab::WhdloadLibrary),
    ("Settings...", LauncherTab::Whdload),
];

pub(super) const CREATE_NAV: &[(&str, LauncherTab)] = &[
    ("Floppy Disk", LauncherTab::CreateFloppy),
    ("Hard Disk", LauncherTab::CreateHard),
];

/// The I/O Ports categories, left to right. `IoPorts` is the default,
/// so its button reads "Serial Port".
pub(super) const IO_NAV: &[(&str, LauncherTab)] = &[
    ("Serial Port", LauncherTab::IoPorts),
    ("Parallel Port", LauncherTab::IoParallel),
    ("Networking", LauncherTab::IoNetworking),
    ("Audio", LauncherTab::IoAudio),
];

/// The A/V & Emu categories, left to right (matching "A/V"). `AvAudio` is the
/// default, so its button reads "Audio".
pub(super) const AV_NAV: &[(&str, LauncherTab)] = &[
    ("Audio", LauncherTab::AvAudio),
    ("Video", LauncherTab::AvVideo),
    ("Display", LauncherTab::AvDisplay),
    ("Emulation", LauncherTab::AvEmulation),
    // Four to a row, so this one wraps onto a second.
    ("Paths", LauncherTab::AvPaths),
];

/// A single editable setting. Parameter-free variants keep the per-tab row
/// tables and `UiControl` hit-testing simple (every control is one `Copy` enum
/// value); the floppy/SCSI families are spelled out rather than indexed for the
/// same reason.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LauncherField {
    // --- the Paths page ---------------------------------------------
    //
    // The configuration's `[paths]` section, edited here and written out
    // with the rest of it. On `MachineSetup` because that is the
    // launcher's edit buffer for everything a configuration holds.
    PathsBase,
    PathsStates,
    PathsScreenshots,
    PathsRecordings,
    PathsNvram,
    PathsTraces,
    PathsConfigs,
    PathsRoms,
    PathsFloppies,
    PathsHarddrives,
    PathsCds,
    // Create Image workshop -- these edit no machine setting, only what the
    // next image will be made of.
    NewFloppyDensity,
    NewFloppyContainer,
    NewFloppyFs,
    NewFloppyFsVariant,
    NewFloppyLabel,
    NewFloppyBootable,
    NewFloppyCreate,
    NewHardSize,
    NewHardGeometryMode,
    NewHardPartitioning,
    NewHardDevice,
    NewHardFs,
    NewHardFsVariant,
    NewHardLabel,
    NewHardBootable,
    NewHardBootPri,
    NewHardReadOnly,
    NewHardSparse,
    NewHardCreate,
    NewGeomCylinders,
    NewGeomSurfaces,
    NewGeomSectors,
    NewGeomReserved,
    NewGeomVendor,
    NewGeomProduct,
    NewGeomRevision,
    NewGeomSave,
    NewGeomAuto,
    // Per-session connection settings.
    NetplayEnabled,
    NetplayMode,
    NetplayRelay,
    NetplayRelayOnly,
    NetplayBind,
    NetplayPeer,
    NetplayPlayer,
    NetplayCode,
    NetplayDelay,
    NetplayRollback,
    NetplayNewCode,
    NetplayCopyCode,
    // System
    Chipset,
    Agnus,
    Denise,
    Video,
    Rtc,
    Identify,
    Rtg,
    // CPU
    Cpu,
    Fpu,
    Clock,
    Icache,
    Dcache,
    Jit,
    // Memory
    ChipRam,
    FastRam,
    SlowRam,
    RamInit,
    RamPattern,
    MbRam,
    AccelRam,
    Z3Ram,
    // ROM
    Rom,
    ExtendedRom,
    FmvRom,
    // Floppy
    FloppyDrives,
    FloppySpeed,
    Df0Image,
    Df0WriteProtect,
    Df1Image,
    Df1WriteProtect,
    Df2Image,
    Df2WriteProtect,
    Df3Image,
    Df3WriteProtect,
    // The per-drive "use a real drive" tick boxes, and the settings behind the
    // Configure button they reveal. The settings are one set of rows shown for
    // whichever bay is being configured, rather than four copies.
    Df0Bridge,
    Df1Bridge,
    Df2Bridge,
    Df3Bridge,
    /// The greyed heading naming the installed library and its version. Inert:
    /// it labels the page rather than editing anything.
    BridgeLibrary,
    /// A line of the explanation shown in place of the settings when there is
    /// no library to apply them to. Inert, and shared by every such line.
    BridgeLibraryHelp,
    BridgeDevice,
    BridgePort,
    BridgeCable,
    BridgeDensity,
    BridgeReadMode,
    BridgeReplaySpeed,
    // Hard disk
    IdeMaster,
    IdeSlave,
    ScsiController,
    ScsiRom,
    ScsiRomOdd,
    ScsiUnit0,
    ScsiUnit1,
    ScsiUnit2,
    ScsiUnit3,
    ScsiUnit4,
    ScsiUnit5,
    ScsiUnit6,
    // The `[lide]` built-in Zorro II IDE board, on its own Storage sub-page
    // rather than crowding the Storage tab's own 12 rows.
    LideBoard,
    LideRom,
    LideRomBank2,
    LideDrive0,
    LideDrive1,
    LideDrive2,
    LideDrive3,
    // `[copperhf]`: Copperline's own virtual hardfile controller
    // (copperhf.device), on its own Storage sub-page like Lide. No board/ROM
    // row -- the board is always there, built into Copperline itself -- just
    // its seven units.
    CopperhfUnit0,
    CopperhfUnit1,
    CopperhfUnit2,
    CopperhfUnit3,
    CopperhfUnit4,
    CopperhfUnit5,
    CopperhfUnit6,
    // Boot priority sub-page: the synthesized-RDB de_BootPri for each hard-disk
    // drive above, edited on its own page so it does not crowd the Storage tab.
    IdeMasterBoot,
    IdeSlaveBoot,
    ScsiUnit0Boot,
    ScsiUnit1Boot,
    ScsiUnit2Boot,
    ScsiUnit3Boot,
    ScsiUnit4Boot,
    ScsiUnit5Boot,
    ScsiUnit6Boot,
    LideDrive0Boot,
    LideDrive1Boot,
    LideDrive2Boot,
    LideDrive3Boot,
    CopperhfUnit0Boot,
    CopperhfUnit1Boot,
    CopperhfUnit2Boot,
    CopperhfUnit3Boot,
    CopperhfUnit4Boot,
    CopperhfUnit5Boot,
    CopperhfUnit6Boot,
    // Host FS mounts (the GUI edits the first FILESYS_GUI_SLOTS entries)
    Filesys0Dir,
    Filesys0Boot,
    Filesys0ReadOnly,
    Filesys1Dir,
    Filesys1Boot,
    Filesys1ReadOnly,
    Filesys2Dir,
    Filesys2Boot,
    Filesys2ReadOnly,
    Filesys3Dir,
    Filesys3Boot,
    Filesys3ReadOnly,
    // CD
    CdImage,
    CdInsertDelay,
    Cd32Nvram,
    // WHDLoad direct boot (the Storage tab's WHDLoad sub-page)
    WhdloadGame,
    WhdloadKickstarts,
    WhdloadLibrary,
    WhdloadWhdPackage,
    WhdloadSkickPackage,
    WhdloadMachine,
    WhdloadOpenRetro,
    WhdloadEnabled,
    WhdloadGames,
    // Serial. Present only with the `midi` feature, the only build carrying
    // serial rows at all.
    #[cfg(feature = "midi")]
    SerialMode,
    /// The remote `host:port` the port dials in `tcp-connect` mode, typed
    /// into the Serial section's Connect box.
    #[cfg(feature = "midi")]
    SerialConnect,
    /// The local address the port binds in `tcp` mode, typed into the
    /// Serial section's Listen box.
    #[cfg(feature = "midi")]
    SerialListen,
    /// `AT*T1`/`AT*T0`'s default at power-on, edited in the Serial
    /// section's Telnet row (modem mode only).
    #[cfg(feature = "midi")]
    SerialTelnet,
    #[cfg(feature = "midi")]
    MidiOut,
    Mt32ControlRom,
    Mt32PcmRom,
    Mt32Panel,
    Mt32Lcd,
    #[cfg(feature = "midi")]
    MidiIn,
    /// Coppersynth's soundfont (.sf2); unset means the bundled
    /// default's search path.
    #[cfg(feature = "coppersynth")]
    CsynthSoundfont,
    CsynthPanel,
    /// The MT-32 mode of Coppersynth: Auto / On / Off.
    #[cfg(feature = "coppersynth")]
    CsynthMt32Mode,
    // Parallel
    ParallelDevice,
    ParallelOutput,
    SamplerInput,
    SamplerGain,
    /// The A2065 Ethernet board: absent, or fitted with a chosen host
    /// backend (isolated / loopback / NAT).
    Ethernet,
    /// Host adapter used while the A2065 backend is bridged.
    EthernetInterface,
    /// The bundled HostSocket bsdsocket.library board: absent, or fitted
    /// with a chosen host backend.
    HostSocket,
    /// Host adapter used while the HostSocket backend is bridged.
    HostSocketInterface,
    /// The MacroSystem Toccata sound board: fitted or not (`[toccata]
    /// enabled`). No other options exist (see docs/internals/toccata.md).
    Toccata,
    /// The freezer cartridge (`[cartridge] model`): none, or the bundled
    /// HRTMon monitor. An image of the user's own (`rom`) is carried
    /// across the round trip but has no row.
    Cartridge,
    /// The MHI virtual MPEG audio decoder board: fitted or not (`[mhi]
    /// enabled`). No other options exist (see docs/internals/mhi.md). Present
    /// only in an `mhi` build, the only build that can fit the board.
    #[cfg(feature = "mhi")]
    Mhi,
    /// Inert field for a non-interactive [`RowKind::SectionHeader`] row.
    SectionHeader,
    // A/V and emulation
    AudioDevice,
    AudioChannelMode,
    AudioStereoSeparation,
    AudioFilter,
    Overscan,
    PixelAspect,
    Scaling,
    Autocrop,
    Tint,
    Deinterlace,
    Phosphor,
    Shader,
    ShaderStrength,
    Bezel,
    PerfOverlay,
    MenuScale,
    StartFullscreen,
    ShowStatusBar,
    FloppySounds,
    FloppyVolume,
    PowerOn,
    AutoLaunch,
    PacingBudget,
    RealtimePriority,
    Warp,
    WarpBoot,
    WarpBootIdle,
    // Input
    Joystick,
    MouseSensitivity,
    MouseCapture,
    Port1Device,
    Port2Device,
}

/// How a row's value is edited, and therefore which widget the panel draws.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowKind {
    /// A `[<] value [>]` picker / stepper.
    Cycle,
    /// A `[<] value [>]` stepper whose value is also a text field: the arrows
    /// nudge it by one and clicking the value types an exact number. Used for
    /// the hard-disk boot priorities, where any value in -128..=127 is valid.
    Bootpri,
    /// An On/Off button.
    Toggle,
    /// A file path with Browse/Clear buttons.
    Path,
    /// A hard-drive image: a path with Browse/Clear, plus an editable
    /// volume-name field (used when the image is a host directory).
    Drive,
    /// One line of a ROM row's identification -- the row's label says
    /// which fact it carries (Name, Version, Revision) and draws as a
    /// greyed prefix, with the value in full text colour after it.
    /// Blank after the prefix when the image is unrecognised.
    RomInfo,
    /// A non-interactive greyed heading that groups the rows beneath it
    /// (e.g. the `Serial:` / `Parallel:` sections of the I/O Ports tab). Its
    /// `field` is inert.
    SectionHeader,
    /// The greyed `Drive` / `Priority` / `Status` column titles above the Boot
    /// Priority rows. Non-interactive; its `field` is inert.
    BootpriHeader,
    /// A floppy drive's media row: an image path with Browse/Clear, or the
    /// real interface in use with a Configure button once bridged.
    FloppyMedia,
    /// The pair of tick boxes under a drive: write protect, and whether the
    /// bay uses a real drive. Its `field` is the drive's write-protect field.
    FloppyFlags,
    /// A free-text value box: click it to type. Unlike [`RowKind::Path`] it
    /// holds a word rather than a file, so it has no Browse button.
    Text,
    /// A button that does something, drawn where a value would be. The row
    /// label is blank and the button carries the wording.
    Action,
    /// A typed number with the unit it is in written beside it; clicking
    /// the unit swaps it. Used for the hard-drive size, where the useful
    /// range is far too wide for a stepper.
    Size,
    /// The geometry mode: Auto and Custom side by side, with a Configure
    /// button appearing beside them once Custom is chosen.
    GeometryMode,
    /// A typed whole number in a plain box, lined up with the value column.
    /// Used where the useful range is too wide to walk with arrows.
    Number,
    /// A typed whole number with a stepper either side, for the geometry
    /// figures: the arrows nudge by one, the box takes an exact value.
    Stepper,
    /// The filesystem family, as a row of tick boxes: which handler the
    /// volume is for, one of them always chosen.
    FsFamily,
    /// The filesystem variant, on the row directly under the family: the
    /// options AmigaDOS's own filesystem carries, greyed for a family that
    /// has none.
    FsVariant,
    /// An account: whether this session is signed in, with the button that
    /// signs it in where a Browse would be. Nothing is stored, so the row
    /// reports the session rather than a setting.
    Account,
}

/// One settings row: a label, the field it edits, and how to edit it.
#[derive(Debug, Clone, Copy)]
pub struct Row {
    pub field: LauncherField,
    pub label: &'static str,
    pub kind: RowKind,
}

pub(super) const fn row(field: LauncherField, label: &'static str, kind: RowKind) -> Row {
    Row { field, label, kind }
}

/// Whether a field is one of the boot-priority steppers, whose range
/// runs to hundreds and so wants the held ramp rather than the steady
/// pace the shorter lists take.
pub fn field_is_bootpri(field: LauncherField) -> bool {
    BOOTPRI_ROWS
        .iter()
        .any(|r| r.field == field && r.kind == RowKind::Bootpri)
}

/// A non-interactive section heading row (see [`RowKind::SectionHeader`]).
pub(super) const fn section_header(label: &'static str) -> Row {
    Row {
        field: F::SectionHeader,
        label,
        kind: RowKind::SectionHeader,
    }
}

/// How many drives one Boot Priority page holds. Nine is a machine without a
/// Lide board -- two IDE bays and a full seven-unit SCSI chain -- and it is
/// also as many rows as the panel has room for under the column titles with
/// the "Info:" note still below them. A Lide board's drives take the list
/// past it, and onto a second page.
pub const BOOTPRI_PAGE_ROWS: usize = 9;

/// The greyed column-title row on the Boot Priority page (see
/// [`RowKind::BootpriHeader`]).
pub(super) const fn bootpri_header() -> Row {
    Row {
        field: F::SectionHeader,
        label: "",
        kind: RowKind::BootpriHeader,
    }
}

/// How many `[[filesys]]` mounts the launcher edits (the config file
/// accepts more; extras round-trip untouched).
pub const FILESYS_GUI_SLOTS: usize = 4;

/// The Host FS mount slot a launcher field addresses, or `None` for other
/// fields: (mount index, whether the field is the boot-priority row).
pub(super) fn filesys_slot(field: LauncherField) -> Option<(usize, bool)> {
    Some(match field {
        LauncherField::Filesys0Dir => (0, false),
        LauncherField::Filesys0Boot => (0, true),
        LauncherField::Filesys1Dir => (1, false),
        LauncherField::Filesys1Boot => (1, true),
        LauncherField::Filesys2Dir => (2, false),
        LauncherField::Filesys2Boot => (2, true),
        LauncherField::Filesys3Dir => (3, false),
        LauncherField::Filesys3Boot => (3, true),
        _ => return None,
    })
}

/// The Host FS mount slot of an Access (read-only) spinner field.
pub(super) fn filesys_readonly_slot(field: LauncherField) -> Option<usize> {
    Some(match field {
        LauncherField::Filesys0ReadOnly => 0,
        LauncherField::Filesys1ReadOnly => 1,
        LauncherField::Filesys2ReadOnly => 2,
        LauncherField::Filesys3ReadOnly => 3,
        _ => return None,
    })
}

impl LauncherField {
    /// Whether this field is a Host FS mount's directory (folder picker),
    /// as opposed to a boot-priority stepper or any other field.
    pub fn is_filesys_dir_field(self) -> bool {
        matches!(filesys_slot(self), Some((_, false)))
    }

    /// Whether this field is a row on the Paths page: a directory in
    /// `[paths]` rather than anything belonging to the machine. They get
    /// a folder picker, they show the whole path rather than a file name,
    /// and they take effect the moment they change.
    pub fn is_paths_field(self) -> bool {
        matches!(
            self,
            LauncherField::PathsBase
                | LauncherField::PathsStates
                | LauncherField::PathsScreenshots
                | LauncherField::PathsRecordings
                | LauncherField::PathsNvram
                | LauncherField::PathsTraces
                | LauncherField::PathsConfigs
                | LauncherField::PathsRoms
                | LauncherField::PathsFloppies
                | LauncherField::PathsHarddrives
                | LauncherField::PathsCds
        )
    }

    /// Whether this field is a WHDLoad staging directory (folder picker):
    /// the Kickstart-image and game-library directories, but not the game
    /// package (an `.lha` file, picked as a file).
    pub fn is_whdload_dir_field(self) -> bool {
        matches!(
            self,
            LauncherField::WhdloadKickstarts | LauncherField::WhdloadLibrary
        ) || cfg!(feature = "game-library") && self.is_whdload_games_field()
    }

    /// The game library, which is a folder of packages rather than one of
    /// them. Its own predicate because the field only exists in a build
    /// with the library, and `matches!` cannot be written conditionally.
    pub fn is_whdload_games_field(self) -> bool {
        #[cfg(feature = "game-library")]
        {
            self == LauncherField::WhdloadGames
        }
        #[cfg(not(feature = "game-library"))]
        {
            false
        }
    }

    /// Whether this field names a `.lha` archive rather than a directory:
    /// the game to launch, and the two support packages.
    pub fn is_whdload_archive_field(self) -> bool {
        matches!(
            self,
            LauncherField::WhdloadGame
                | LauncherField::WhdloadWhdPackage
                | LauncherField::WhdloadSkickPackage
        )
    }

    /// Whether this field is one of the WHDLoad host paths (game package or
    /// staging directory), which show the whole host path like the Host FS
    /// mounts do.
    pub fn is_whdload_path_field(self) -> bool {
        matches!(
            self,
            LauncherField::WhdloadGame
                | LauncherField::WhdloadGames
                | LauncherField::WhdloadKickstarts
                | LauncherField::WhdloadLibrary
                | LauncherField::WhdloadWhdPackage
                | LauncherField::WhdloadSkickPackage
        )
    }
}

/// What a hard-disk slot holds. The two are interchangeable to everything
/// that only wants to know whether the slot is occupied.
pub(super) enum DriveContents<'a> {
    Image(&'a Path),
    HostDisk,
}

/// The `[cartridge] model` spelling of a launcher choice.
pub(super) fn cartridge_model_name(
    model: Option<crate::cartridge::CartridgeModel>,
) -> &'static str {
    match model {
        None => "none",
        Some(m) => m.label(),
    }
}

/// The row label for a cartridge choice.
pub(super) fn cartridge_label(model: Option<crate::cartridge::CartridgeModel>) -> &'static str {
    model.map_or("None", |m| m.display_name())
}

pub(super) const SYSTEM_ROWS: [Row; 8] = [
    row(F::Chipset, "Chipset", Cycle),
    row(F::Agnus, "Agnus", Cycle),
    row(F::Denise, "Denise", Cycle),
    row(F::Video, "Video", Cycle),
    row(F::Rtc, "Real-time clock", Cycle),
    row(F::Identify, "Identify board", Cycle),
    row(F::Rtg, "RTG card", Cycle),
    row(F::Cartridge, "Freezer cartridge", Cycle),
];
pub(super) const CPU_ROWS: [Row; 6] = [
    row(F::Cpu, "CPU", Cycle),
    row(F::Fpu, "FPU (68881/2)", Cycle),
    row(F::Clock, "Clock", Cycle),
    row(F::Icache, "Instruction cache", Cycle),
    row(F::Dcache, "Data cache", Cycle),
    row(F::Jit, "JIT accelerator", Cycle),
];
pub(super) const MEMORY_ROWS: [Row; 8] = [
    row(F::ChipRam, "Chip RAM", Cycle),
    row(F::FastRam, "Fast RAM", Cycle),
    row(F::SlowRam, "Slow RAM", Cycle),
    // Below the sizes most people came for: what the bits hold at
    // power-on, and (only while the fill is Fixed) the word they hold.
    row(F::RamInit, "Power-on fill", Cycle),
    row(F::RamPattern, "Fill pattern", RowKind::Text),
    row(F::MbRam, "Motherboard RAM", Cycle),
    row(F::AccelRam, "Accelerator RAM", Cycle),
    row(F::Z3Ram, "Zorro III RAM", Cycle),
];
// The Kickstart row carries its identification beneath it -- what the
// chosen image checksums to, split into Name / Version / Revision lines
// ("Kickstart", "3.1", "40.68"), since a ROM file's name says only what
// its dumper called it. The lines stand whether or not an image is
// loaded; a blank value means an empty (or unrecognised) slot.
pub(super) const ROM_ROWS: [Row; 9] = [
    section_header("Primary ROM:"),
    row(F::Rom, "  Kickstart ROM", PathRow),
    // The label picks which fact the line carries.
    row(F::Rom, "Name", RowKind::RomInfo),
    row(F::Rom, "Version", RowKind::RomInfo),
    row(F::Rom, "Revision", RowKind::RomInfo),
    section_header("Extended ROM:"),
    row(F::ExtendedRom, "  Extended ROM", PathRow),
    section_header("CD32 Full Motion Video:"),
    row(F::FmvRom, "  FMV module ROM", PathRow),
];
// Each drive is a greyed "DFn:" heading with its settings indented under it. The
// heading is keyed on the drive's image field so `row_hidden` drops it along
// with the drive's rows when the drive is not wired in.
// Each wired drive is a greyed "DFn:" heading, its media row, then the two
// tick boxes that share a line beneath. The media row shows an image path with
// Browse/Clear, or -- once FluxBridge is ticked -- the real interface in use
// with a Configure button onto its settings.
pub(super) const FLOPPY_ROWS: [Row; 14] = [
    row(F::FloppyDrives, "Drives", Cycle),
    row(F::FloppySpeed, "Drive speed", Cycle),
    row(F::Df0Image, "DF0:", RowKind::SectionHeader),
    row(F::Df0Image, "  Disk image", RowKind::FloppyMedia),
    row(F::Df0WriteProtect, "", RowKind::FloppyFlags),
    row(F::Df1Image, "DF1:", RowKind::SectionHeader),
    row(F::Df1Image, "  Disk image", RowKind::FloppyMedia),
    row(F::Df1WriteProtect, "", RowKind::FloppyFlags),
    row(F::Df2Image, "DF2:", RowKind::SectionHeader),
    row(F::Df2Image, "  Disk image", RowKind::FloppyMedia),
    row(F::Df2WriteProtect, "", RowKind::FloppyFlags),
    row(F::Df3Image, "DF3:", RowKind::SectionHeader),
    row(F::Df3Image, "  Disk image", RowKind::FloppyMedia),
    row(F::Df3WriteProtect, "", RowKind::FloppyFlags),
];
/// The FluxBridge settings page, shown for whichever bay was configured.
#[cfg(feature = "fluxbridge")]
pub(super) const FLOPPY_BRIDGE_ROWS: [Row; 7] = [
    // Inert: the label is built from the loaded library's version (see
    // `bridge_library_heading`), so the text here is never drawn.
    row(F::BridgeLibrary, "", RowKind::SectionHeader),
    row(F::BridgeDevice, "Interface", Cycle),
    row(F::BridgePort, "Serial port", Cycle),
    row(F::BridgeCable, "Drive select", Cycle),
    row(F::BridgeDensity, "Density", Cycle),
    row(F::BridgeReadMode, "Read mode", Cycle),
    row(F::BridgeReplaySpeed, "Replay speed", Cycle),
];
pub(super) const STORAGE_ROWS: [Row; 12] = [
    row(F::IdeMaster, "IDE master", Drive),
    row(F::IdeSlave, "IDE slave", Drive),
    row(F::ScsiController, "SCSI controller", Cycle),
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
pub(super) const HOSTFS_ROWS: [Row; 12] = [
    row(F::Filesys0Dir, "HOSTFS0", Drive),
    row(F::Filesys0Boot, "  Boot priority", Cycle),
    row(F::Filesys0ReadOnly, "  Access", Cycle),
    row(F::Filesys1Dir, "HOSTFS1", Drive),
    row(F::Filesys1Boot, "  Boot priority", Cycle),
    row(F::Filesys1ReadOnly, "  Access", Cycle),
    row(F::Filesys2Dir, "HOSTFS2", Drive),
    row(F::Filesys2Boot, "  Boot priority", Cycle),
    row(F::Filesys2ReadOnly, "  Access", Cycle),
    row(F::Filesys3Dir, "HOSTFS3", Drive),
    row(F::Filesys3Boot, "  Boot priority", Cycle),
    row(F::Filesys3ReadOnly, "  Access", Cycle),
];
// One boot-priority row per hard-disk drive. The IDE bays stand their
// ground greyed when empty; a SCSI unit or Lide slot is listed only once it
// carries a disk (`row_hidden`). More rows than one page holds run onto a
// second page -- see `MachineSetup::boot_page_of`.
pub(super) const BOOTPRI_ROWS: [Row; 20] = [
    row(F::IdeMasterBoot, "IDE master", Bootpri),
    row(F::IdeSlaveBoot, "IDE slave", Bootpri),
    row(F::ScsiUnit0Boot, "SCSI unit 0", Bootpri),
    row(F::ScsiUnit1Boot, "SCSI unit 1", Bootpri),
    row(F::ScsiUnit2Boot, "SCSI unit 2", Bootpri),
    row(F::ScsiUnit3Boot, "SCSI unit 3", Bootpri),
    row(F::ScsiUnit4Boot, "SCSI unit 4", Bootpri),
    row(F::ScsiUnit5Boot, "SCSI unit 5", Bootpri),
    row(F::ScsiUnit6Boot, "SCSI unit 6", Bootpri),
    // The Zorro IDE board's drives sit below the motherboard's and the
    // SCSI units. The cascade default ranks by this order too, though it
    // seeds each priority when its drive is added -- drives filled out of
    // table order can still tie, as they always could across families.
    row(F::LideDrive0Boot, "Lide drive 0", Bootpri),
    row(F::LideDrive1Boot, "Lide drive 1", Bootpri),
    row(F::LideDrive2Boot, "Lide drive 2", Bootpri),
    row(F::LideDrive3Boot, "Lide drive 3", Bootpri),
    // copperhf.device's units sit last: a Copperline-only board with no
    // real-hardware counterpart, ranked after every board with one.
    row(F::CopperhfUnit0Boot, "copperhf unit 0", Bootpri),
    row(F::CopperhfUnit1Boot, "copperhf unit 1", Bootpri),
    row(F::CopperhfUnit2Boot, "copperhf unit 2", Bootpri),
    row(F::CopperhfUnit3Boot, "copperhf unit 3", Bootpri),
    row(F::CopperhfUnit4Boot, "copperhf unit 4", Bootpri),
    row(F::CopperhfUnit5Boot, "copperhf unit 5", Bootpri),
    row(F::CopperhfUnit6Boot, "copperhf unit 6", Bootpri),
];
pub(super) const CD_ROWS: [Row; 3] = [
    row(F::CdImage, "CD image", PathRow),
    row(F::CdInsertDelay, "Insert delay", Cycle),
    row(F::Cd32Nvram, "CD32 NVRAM", PathRow),
];
// The `[lide]` Storage sub-page: board personality, boot ROM(s), and up to
// four drives (RIPPLE's two channels; RIDE/AT-Bus 2008 hide slots 2-3, and
// AT-Bus 2008 also hides the second ROM bank -- it has no flash banking).
// The drives' boot priorities live on the shared Boot Priority page with
// every other drive's, in `BOOTPRI_ROWS`.
pub(super) const LIDE_ROWS: [Row; 7] = [
    row(F::LideBoard, "Board", Cycle),
    row(F::LideRom, "Boot ROM", PathRow),
    row(F::LideRomBank2, "Boot ROM bank 2", PathRow),
    row(F::LideDrive0, "Drive 0", Drive),
    row(F::LideDrive1, "Drive 1", Drive),
    row(F::LideDrive2, "Drive 2", Drive),
    row(F::LideDrive3, "Drive 3", Drive),
];
// The `[copperhf]` Storage sub-page: copperhf.device's seven units, no
// board/ROM row -- the board is always there, built into Copperline itself
// (see `RawCopperhf`'s own doc comment) -- so unlike Lide there is nothing
// to fit before the drives appear. Boot priorities live on the shared Boot
// Priority page with every other drive's, in `BOOTPRI_ROWS`.
pub(super) const COPPERHF_ROWS: [Row; 7] = [
    row(F::CopperhfUnit0, "Unit 0", Drive),
    row(F::CopperhfUnit1, "Unit 1", Drive),
    row(F::CopperhfUnit2, "Unit 2", Drive),
    row(F::CopperhfUnit3, "Unit 3", Drive),
    row(F::CopperhfUnit4, "Unit 4", Drive),
    row(F::CopperhfUnit5, "Unit 5", Drive),
    row(F::CopperhfUnit6, "Unit 6", Drive),
];
// The WHDLoad Settings page: the game to launch, then what staging
// draws on (src/whdload.rs). Drive rows like the Host FS mounts so the
// whole host path shows; the staged volumes mount under fixed names
// (WHDBoot:/WHDGame:), so there is no volume box to fill.
//
// Pinning, the account and the game folder belong to the game library, so
// they are only here in a build that has one. Their settings still
// round-trip through a save either way -- a configuration written by a full
// build loads in a slim one without losing them.
#[cfg(not(feature = "game-library"))]
pub(super) const WHDLOAD_ROWS: [Row; 8] = [
    section_header("WHDLoad Settings:"),
    // What to boot, and how: what a person changes per game.
    row(F::WhdloadGame, "Launch game", Drive),
    row(F::WhdloadMachine, "Machine type", Cycle),
    // Then the places things live, set once and left.
    section_header("Directories:"),
    row(F::WhdloadWhdPackage, "WHDLoad package", Drive),
    row(F::WhdloadSkickPackage, "SKick package", Drive),
    row(F::WhdloadKickstarts, "Kickstart ROMs", Drive),
    row(F::WhdloadLibrary, "Save data", Drive),
];
#[cfg(feature = "game-library")]
pub(super) const WHDLOAD_ROWS: [Row; 10] = [
    section_header("WHDLoad Settings:"),
    // What to boot, and how: what a person changes per game.
    row(F::WhdloadGame, "Launch game", Drive),
    row(F::WhdloadMachine, "Machine type", Cycle),
    row(F::WhdloadOpenRetro, "OpenRetro", RowKind::Account),
    // Then the places things live, set once and left.
    section_header("Directories:"),
    row(F::WhdloadWhdPackage, "WHDLoad package", Drive),
    row(F::WhdloadSkickPackage, "SKick package", Drive),
    row(F::WhdloadKickstarts, "Kickstart ROMs", Drive),
    row(F::WhdloadGames, "Game library", Drive),
    row(F::WhdloadLibrary, "Save data", Drive),
];
// The MIDI endpoint rows appear only when the serial port is in MIDI mode, so
// the Serial section shows just the Device / Mode selector otherwise. The
// selector is labelled "Device / Mode" because some choices are devices (MIDI)
// and some are modes (stdout, PTY, TCP).
// Rows under each I/O Ports section heading are indented two spaces so they
// read as belonging to their `Serial:` / `Parallel:` / `Ethernet:` port.
#[cfg(feature = "midi")]
pub(super) const SERIAL_ROWS_BASE: [Row; 1] = [row(F::SerialMode, "  Device / Mode", Cycle)];
// The two TCP modes each carry one address, and only one: dialling out needs
// somewhere to dial, listening needs somewhere to bind. Each box shows only
// under the mode it belongs to, so neither mode offers the other's address.
#[cfg(feature = "midi")]
pub(super) const SERIAL_ROWS_TCP_CONNECT: [Row; 2] = [
    row(F::SerialMode, "  Device / Mode", Cycle),
    row(F::SerialConnect, "  Connect", RowKind::Text),
];
#[cfg(feature = "midi")]
pub(super) const SERIAL_ROWS_TCP_LISTEN: [Row; 2] = [
    row(F::SerialMode, "  Device / Mode", Cycle),
    row(F::SerialListen, "  Listen", RowKind::Text),
];
// The modem's own rows: where it listens for incoming calls (RING/ATA/S0
// answer them) and whether telnet NVT translation (AT*T1) is on by default.
// The phonebook is config-file-only -- no row for it.
#[cfg(feature = "midi")]
pub(super) const SERIAL_ROWS_MODEM: [Row; 3] = [
    row(F::SerialMode, "  Device / Mode", Cycle),
    row(F::SerialListen, "  Listen", RowKind::Text),
    row(F::SerialTelnet, "  Telnet", Cycle),
];
#[cfg(feature = "midi")]
pub(super) const SERIAL_ROWS_MIDI: [Row; 3] = [
    row(F::SerialMode, "  Device / Mode", Cycle),
    row(F::MidiIn, "  MIDI input", Cycle),
    row(F::MidiOut, "  MIDI output", Cycle),
];
// Picking MT-32 as the output adds the two ROM images it runs on and
// its front panel; nothing else needs them, so nothing else shows them.
#[cfg(all(feature = "midi", feature = "mt32"))]
pub(super) const SERIAL_ROWS_MT32: [Row; 7] = [
    row(F::SerialMode, "  Device / Mode", Cycle),
    row(F::MidiIn, "  MIDI input", Cycle),
    row(F::MidiOut, "  MIDI output", Cycle),
    row(F::Mt32ControlRom, "  Control ROM", PathRow),
    row(F::Mt32PcmRom, "  PCM ROM", PathRow),
    row(F::Mt32Panel, "  Front panel", Cycle),
    row(F::Mt32Lcd, "  Display", Cycle),
];
// Coppersynth needs no ROMs: its rows are the soundfont it
// plays and whether the MT-32 translation layer sits in front of it.
#[cfg(all(feature = "midi", feature = "coppersynth"))]
pub(super) const SERIAL_ROWS_CSYNTH: [Row; 6] = [
    row(F::SerialMode, "  Device / Mode", Cycle),
    row(F::MidiIn, "  MIDI input", Cycle),
    row(F::MidiOut, "  MIDI output", Cycle),
    row(F::CsynthSoundfont, "  SoundFont", PathRow),
    row(F::CsynthPanel, "  Front panel", Cycle),
    row(F::CsynthMt32Mode, "  MT-32 mode", Cycle),
];
// The sampler input/gain rows appear only when the sampler is the selected
// device, so None/Printer show just the Device selector.
pub(super) const PARALLEL_ROWS_BASE: [Row; 1] = [row(F::ParallelDevice, "  Device", Cycle)];
// The printer adds a capture-file picker; the sampler adds its input/gain rows.
pub(super) const PARALLEL_ROWS_PRINTER: [Row; 2] = [
    row(F::ParallelDevice, "  Device", Cycle),
    row(F::ParallelOutput, "  Output file", PathRow),
];
pub(super) const PARALLEL_ROWS_SAMPLER: [Row; 3] = [
    row(F::ParallelDevice, "  Device", Cycle),
    row(F::SamplerInput, "  Audio input", Cycle),
    row(F::SamplerGain, "  Input gain", Cycle),
];
pub(super) const ETHERNET_ROWS: [Row; 4] = [
    row(F::Ethernet, "  A2065", Cycle),
    row(F::EthernetInterface, "  Host adapter", Cycle),
    row(F::HostSocket, "  HostSocket", Cycle),
    row(F::HostSocketInterface, "  Host adapter", Cycle),
];
// Both boards are a single fit/don't-fit toggle -- no host backend, no other
// options (see docs/internals/toccata.md and docs/internals/mhi.md). Host
// audio capture/backend settings (wav capture, stems, device selection)
// intentionally stay command-line/config-file only and have no row here.
#[cfg(feature = "mhi")]
pub(super) const SOUND_ROWS: [Row; 2] = [
    row(F::Toccata, "  Toccata", Cycle),
    row(F::Mhi, "  MHI decoder", Cycle),
];
#[cfg(not(feature = "mhi"))]
pub(super) const SOUND_ROWS: [Row; 1] = [row(F::Toccata, "  Toccata", Cycle)];
// The A/V & Emu tab is split into five categories switched via the top nav row.
// The Video category also carries the CRT-shader controls (a picture setting).
// The emulated picture, in signal order -- what the monitor is fed and how
// it is drawn -- with the shader pair last, since strength greys off the
// shader. The host window's own settings are DISPLAY_ROWS.
pub(super) const VIDEO_ROWS: [Row; 10] = [
    row(F::Bezel, "Monitor bezel", Cycle),
    row(F::Overscan, "Overscan", Cycle),
    row(F::PixelAspect, "Pixel aspect", Cycle),
    row(F::Scaling, "Scaling", Cycle),
    row(F::Autocrop, "Autocrop", Cycle),
    row(F::Deinterlace, "Deinterlace", Cycle),
    row(F::Tint, "Screen tint", Cycle),
    row(F::Phosphor, "Phosphor", Cycle),
    row(F::Shader, "CRT shader", Cycle),
    row(F::ShaderStrength, "Shader strength", Cycle),
];

// The host window and its furniture, as distinct from the picture inside it.
pub(super) const DISPLAY_ROWS: [Row; 4] = [
    row(F::StartFullscreen, "Start fullscreen", Cycle),
    row(F::ShowStatusBar, "Status bar", Cycle),
    row(F::PerfOverlay, "Perf overlay", Cycle),
    row(F::MenuScale, "Menu size", Cycle),
];
pub(super) const AUDIO_ROWS: [Row; 6] = [
    row(F::AudioDevice, "Audio output", Cycle),
    row(F::AudioChannelMode, "Channel mode", Cycle),
    row(F::AudioStereoSeparation, "Stereo separation", Cycle),
    row(F::AudioFilter, "Audio filter", Cycle),
    row(F::FloppySounds, "Floppy sounds", Cycle),
    row(F::FloppyVolume, "Floppy volume", Cycle),
];
#[cfg(not(feature = "game-library"))]
pub(super) const EMULATION_ROWS: [Row; 7] = [
    row(F::PowerOn, "Power on startup", Cycle),
    row(F::AutoLaunch, "Run on startup", Cycle),
    row(F::RealtimePriority, "Realtime priority", Cycle),
    row(F::PacingBudget, "Pacing budget", Cycle),
    row(F::Warp, "Warp speed", Cycle),
    row(F::WarpBoot, "Warp boot", Cycle),
    row(F::WarpBootIdle, "Warp boot idle", Cycle),
];
#[cfg(feature = "game-library")]
pub(super) const EMULATION_ROWS: [Row; 8] = [
    row(F::PowerOn, "Power on startup", Cycle),
    row(F::AutoLaunch, "Run on startup", Cycle),
    row(F::RealtimePriority, "Realtime priority", Cycle),
    row(F::PacingBudget, "Pacing budget", Cycle),
    row(F::Warp, "Warp speed", Cycle),
    row(F::WarpBoot, "Warp boot", Cycle),
    row(F::WarpBootIdle, "Warp boot idle", Cycle),
    // Off, the strip loses its WHDLoad entry and the pages behind it stop
    // doing anything at all -- no database read, no cover worker, no scan.
    row(F::WhdloadEnabled, "WHDLoad", Cycle),
];
/// The Paths page. Every row is optional: cleared, it inherits, and the
/// value shown is the directory that would be used. Nothing here describes
/// the machine, so none of it round-trips through [`RawConfig`].
pub(super) const PATHS_ROWS: [Row; 12] = [
    row(F::PathsBase, "Base folder", PathRow),
    section_header("Custom directories:"),
    // Indented under the heading, the same as the sections on the I/O
    // Ports and MT-32 pages: the base above it is not one of these, and
    // the indent is what says so.
    row(F::PathsStates, "  Save states", PathRow),
    row(F::PathsScreenshots, "  Screenshots", PathRow),
    row(F::PathsRecordings, "  Recordings", PathRow),
    row(F::PathsNvram, "  NVRAM", PathRow),
    row(F::PathsTraces, "  Traces", PathRow),
    row(F::PathsConfigs, "  Config files", PathRow),
    row(F::PathsRoms, "  ROMs", PathRow),
    row(F::PathsFloppies, "  Floppies", PathRow),
    row(F::PathsHarddrives, "  Hard drives", PathRow),
    row(F::PathsCds, "  CD images", PathRow),
];

/// The floppy page. Every option the format carries is on it; nothing on
/// it reads or writes the machine's configuration.
pub(super) const NEW_FLOPPY_ROWS: [Row; 8] = [
    section_header("Create Floppy Disk image (ADF):"),
    row(F::NewFloppyDensity, "Density", Cycle),
    row(F::NewFloppyContainer, "Container", Cycle),
    row(F::NewFloppyFs, "Filesystem", RowKind::FsFamily),
    row(F::NewFloppyFsVariant, "DOSType", RowKind::FsVariant),
    row(F::NewFloppyLabel, "Volume name", RowKind::Text),
    row(F::NewFloppyBootable, "Bootable", Toggle),
    row(F::NewFloppyCreate, "", RowKind::Action),
];

pub(super) const NEW_HARD_ROWS: [Row; 13] = [
    section_header("Create Hard Disk image (HDF):"),
    row(F::NewHardSize, "Size", RowKind::Size),
    row(F::NewHardGeometryMode, "Geometry", RowKind::GeometryMode),
    row(F::NewHardPartitioning, "Partitioning", Cycle),
    row(F::NewHardFs, "Filesystem", RowKind::FsFamily),
    row(F::NewHardFsVariant, "DOSType", RowKind::FsVariant),
    row(F::NewHardDevice, "Device name", RowKind::Text),
    row(F::NewHardLabel, "Volume name", RowKind::Text),
    row(F::NewHardBootable, "Bootable", Toggle),
    row(F::NewHardBootPri, "Boot priority", RowKind::Number),
    row(F::NewHardReadOnly, "Read only", Toggle),
    row(F::NewHardSparse, "Sparse image", Toggle),
    row(F::NewHardCreate, "", RowKind::Action),
];

/// The geometry editor, reached from the hard-disk page.
pub(super) const NEW_GEOMETRY_ROWS: [Row; 10] = [
    section_header("Custom disk geometry:"),
    row(F::NewGeomCylinders, "Cylinders", RowKind::Stepper),
    // The Amiga's own word for it, and the name of the Rigid Disk Block
    // field this ends up in.
    row(F::NewGeomSurfaces, "Surfaces", RowKind::Stepper),
    row(F::NewGeomSectors, "Sectors per track", RowKind::Stepper),
    row(F::NewGeomReserved, "Reserved blocks", RowKind::Stepper),
    // What the drive answers when asked what it is. HDToolBox shows the
    // first two as its Drive and Type columns.
    section_header("Drive identity:"),
    row(F::NewGeomVendor, "Drive", RowKind::Text),
    row(F::NewGeomProduct, "Type", RowKind::Text),
    row(F::NewGeomRevision, "Revision", RowKind::Text),
    row(F::NewGeomSave, "", RowKind::Action),
];

pub(super) const NETPLAY_ROWS: [Row; 9] = [
    row(F::NetplayEnabled, "Netplay", Toggle),
    row(F::NetplayMode, "Connection", Cycle),
    row(F::NetplayPlayer, "Local player", Cycle),
    row(F::NetplayBind, "Local address", RowKind::Text),
    row(F::NetplayPeer, "Peer address", RowKind::Text),
    row(F::NetplayCode, "Session code", RowKind::Text),
    row(F::NetplayDelay, "Input delay", Cycle),
    row(F::NetplayRollback, "Rollback limit", Cycle),
    row(F::NetplayNewCode, "", RowKind::Action),
];

pub(super) const INTERNET_NETPLAY_ROWS: [Row; 9] = [
    row(F::NetplayEnabled, "Netplay", Toggle),
    row(F::NetplayMode, "Connection", Cycle),
    row(F::NetplayPlayer, "Local player", Cycle),
    row(F::NetplayCode, "Invitation", RowKind::Text),
    row(F::NetplayRelay, "Relay server", RowKind::Text),
    row(F::NetplayRelayOnly, "Route", Cycle),
    row(F::NetplayDelay, "Input delay", Cycle),
    row(F::NetplayRollback, "Rollback limit", Cycle),
    row(F::NetplayNewCode, "", RowKind::Action),
];

pub(super) const INPUT_ROWS: [Row; 5] = [
    row(F::Port1Device, "Port 1", Cycle),
    row(F::Port2Device, "Port 2", Cycle),
    row(F::Joystick, "Joystick input", Cycle),
    row(F::MouseSensitivity, "Mouse sensitivity", Cycle),
    row(F::MouseCapture, "Mouse capture", Cycle),
];

/// The rows shown on a tab, top to bottom. Most tabs are fixed and borrow their
/// static row table; only the composed tabs (the Boot Priority page and the
/// dynamic I/O Ports tab) allocate. The I/O Ports tab is
/// dynamic: the MIDI endpoint rows appear only in MIDI mode and the
/// sampler/printer rows only for those devices, so unrelated options stay hidden
/// rather than greyed. The `Zorro` tab has no rows: it is drawn as a board list
/// with Add/Remove controls (see the panel code).
pub fn rows(
    tab: LauncherTab,
    parallel_device: ParallelDevice,
    serial_mode: SerialMode,
    midi_out_is_mt32: bool,
    midi_out_is_csynth: bool,
) -> Cow<'static, [Row]> {
    match tab {
        LauncherTab::CreateFloppy => Cow::Borrowed(&NEW_FLOPPY_ROWS),
        LauncherTab::CreateHard => Cow::Borrowed(&NEW_HARD_ROWS),
        LauncherTab::CreateGeometry => Cow::Borrowed(&NEW_GEOMETRY_ROWS),
        LauncherTab::System => Cow::Borrowed(&SYSTEM_ROWS),
        LauncherTab::Cpu => Cow::Borrowed(&CPU_ROWS),
        LauncherTab::Memory => Cow::Borrowed(&MEMORY_ROWS),
        LauncherTab::Rom => Cow::Borrowed(&ROM_ROWS),
        LauncherTab::Floppy => Cow::Borrowed(&FLOPPY_ROWS),
        // Unreachable without the feature: nothing offers a way in, since the
        // tick box that turns a bay over is not drawn either.
        #[cfg(not(feature = "fluxbridge"))]
        LauncherTab::FluxBridge => Cow::Borrowed(&[]),
        #[cfg(feature = "fluxbridge")]
        LauncherTab::FluxBridge => Cow::Borrowed(&FLOPPY_BRIDGE_ROWS),
        // The Storage tab shows the IDE/SCSI options (the common case). Its
        // sub-page links are a fixed nav row at the top (see the panel code),
        // in the same place as each sub-page's Back button, so they are not part
        // of the row grid.
        LauncherTab::Storage => Cow::Borrowed(&STORAGE_ROWS),
        // Every boot page carries the same table and the same column
        // titles; which drives each one draws is decided per drive by
        // `MachineSetup::boot_page_of`, since only the machine knows which
        // slots are filled.
        LauncherTab::BootPriority | LauncherTab::BootPriorityMore(_) => {
            // The greyed column titles, then one row per hard-disk drive.
            let mut rows = vec![bootpri_header()];
            rows.extend_from_slice(&BOOTPRI_ROWS);
            Cow::Owned(rows)
        }
        LauncherTab::HostFs => Cow::Borrowed(&HOSTFS_ROWS),
        LauncherTab::Whdload => Cow::Borrowed(&WHDLOAD_ROWS),
        // The Library draws a list of games rather than rows of settings.
        #[cfg(feature = "game-library")]
        LauncherTab::WhdloadLibrary => Cow::Borrowed(&[]),
        // Drawn as its own layout: a disk table and its buttons, not rows.
        LauncherTab::HostDisk => Cow::Borrowed(&[]),
        LauncherTab::Cd => Cow::Borrowed(&CD_ROWS),
        LauncherTab::Lide => Cow::Borrowed(&LIDE_ROWS),
        LauncherTab::Copperhf => Cow::Borrowed(&COPPERHF_ROWS),
        LauncherTab::IoPorts => Cow::Owned(io_serial_rows(
            serial_mode,
            midi_out_is_mt32,
            midi_out_is_csynth,
        )),
        LauncherTab::IoParallel => Cow::Owned(io_parallel_rows(parallel_device)),
        LauncherTab::IoNetworking => Cow::Owned(io_networking_rows()),
        LauncherTab::IoAudio => Cow::Owned(io_audio_rows()),
        LauncherTab::Input => Cow::Borrowed(&INPUT_ROWS),
        LauncherTab::Netplay => Cow::Borrowed(&NETPLAY_ROWS),
        LauncherTab::Zorro => Cow::Borrowed(&[]),
        // A/V & Emu defaults to the Audio category; Video and Emulation are its
        // sibling categories, switched via the top nav row.
        LauncherTab::AvAudio => Cow::Borrowed(&AUDIO_ROWS),
        LauncherTab::AvVideo => Cow::Borrowed(&VIDEO_ROWS),
        LauncherTab::AvDisplay => Cow::Borrowed(&DISPLAY_ROWS),
        LauncherTab::AvEmulation => Cow::Borrowed(&EMULATION_ROWS),
        LauncherTab::AvPaths => Cow::Borrowed(&PATHS_ROWS),
    }
}

/// The I/O Ports pages, one section each: `Serial:` (only in a `midi`
/// build, which is the only build with serial rows), `Parallel:`,
/// `Ethernet:` and `Audio:`, each under its greyed heading and each
/// showing only the rows relevant to its selected device/mode.
pub(super) fn io_serial_rows(
    serial_mode: SerialMode,
    midi_out_is_mt32: bool,
    midi_out_is_csynth: bool,
) -> Vec<Row> {
    let mut rows = Vec::new();
    let serial = serial_rows(serial_mode, midi_out_is_mt32, midi_out_is_csynth);
    if !serial.is_empty() {
        rows.push(section_header("Serial:"));
        rows.extend_from_slice(serial);
    }
    rows
}

pub(super) fn io_parallel_rows(parallel_device: ParallelDevice) -> Vec<Row> {
    let mut rows = vec![section_header("Parallel:")];
    rows.extend_from_slice(parallel_rows(parallel_device));
    rows
}

pub(super) fn io_networking_rows() -> Vec<Row> {
    let mut rows = vec![section_header("Ethernet:")];
    rows.extend_from_slice(&ETHERNET_ROWS);
    rows
}

pub(super) fn io_audio_rows() -> Vec<Row> {
    let mut rows = vec![section_header("Sound Card:")];
    rows.extend_from_slice(&SOUND_ROWS);
    rows
}

/// Serial rows for the current mode. Only the `midi` build has any; without it
/// the Serial section is empty and omitted from the I/O Ports tab.
pub(super) fn serial_rows(
    serial_mode: SerialMode,
    midi_out_is_mt32: bool,
    midi_out_is_csynth: bool,
) -> &'static [Row] {
    #[cfg(feature = "midi")]
    {
        if serial_mode != SerialMode::Midi {
            return match serial_mode {
                SerialMode::TcpConnect => &SERIAL_ROWS_TCP_CONNECT,
                SerialMode::Tcp => &SERIAL_ROWS_TCP_LISTEN,
                SerialMode::Modem => &SERIAL_ROWS_MODEM,
                _ => &SERIAL_ROWS_BASE,
            };
        }
        #[cfg(feature = "mt32")]
        if midi_out_is_mt32 {
            return &SERIAL_ROWS_MT32;
        }
        #[cfg(feature = "coppersynth")]
        if midi_out_is_csynth {
            return &SERIAL_ROWS_CSYNTH;
        }
        let _ = (midi_out_is_mt32, midi_out_is_csynth);
        &SERIAL_ROWS_MIDI
    }
    #[cfg(not(feature = "midi"))]
    {
        let _ = (serial_mode, midi_out_is_mt32, midi_out_is_csynth);
        &[]
    }
}

/// Parallel rows for the selected device: the printer adds its output-file
/// picker, the sampler its input and gain; None shows just the Device selector.
pub(super) fn parallel_rows(parallel_device: ParallelDevice) -> &'static [Row] {
    match parallel_device {
        ParallelDevice::Sampler => &PARALLEL_ROWS_SAMPLER,
        ParallelDevice::Printer => &PARALLEL_ROWS_PRINTER,
        ParallelDevice::None => &PARALLEL_ROWS_BASE,
    }
}

/// Machine models offered in the selector strip, roughly chronological.
pub const MODELS: [MachineModel; 10] = [
    MachineModel::A1000,
    MachineModel::A500Ocs,
    MachineModel::A500,
    MachineModel::A500Plus,
    MachineModel::A600,
    MachineModel::A1200,
    MachineModel::A3000,
    MachineModel::A4000,
    MachineModel::Cdtv,
    MachineModel::Cd32,
];

// --- value preset lists for the cycle/stepper controls -------------------

pub(super) const CHIPSETS: [Chipset; 3] = [Chipset::Ocs, Chipset::Ecs, Chipset::Aga];
pub(super) const RTG_CARDS: [RtgCard; 6] = [
    RtgCard::None,
    RtgCard::Picasso2,
    RtgCard::Picasso2Plus,
    RtgCard::GraffityZ2,
    RtgCard::GraffityZ3,
    RtgCard::Z3660,
];
pub(super) const AGNUS_CHOICES: [Option<AgnusRevision>; 5] = [
    None,
    Some(AgnusRevision::Ocs),
    Some(AgnusRevision::Ecs8372Rev4),
    Some(AgnusRevision::Ecs8375),
    Some(AgnusRevision::AgaAlice),
];
pub(super) const DENISE_CHOICES: [Option<DeniseRevision>; 4] = [
    None,
    Some(DeniseRevision::Ocs),
    Some(DeniseRevision::Ecs8373),
    Some(DeniseRevision::AgaLisa),
];
pub(super) const VIDEO_CHOICES: [VideoStandard; 2] = [VideoStandard::Pal, VideoStandard::Ntsc];
pub(super) const CPUS: [CpuModel; 7] = [
    CpuModel::M68000,
    CpuModel::M68010,
    CpuModel::M68EC020,
    CpuModel::M68020,
    CpuModel::M68030,
    CpuModel::M68040,
    CpuModel::M68060,
];
/// Storage-idle thresholds for the Warp boot row, in emulated seconds.
/// The threshold must outlast the boot's longest storage-quiet stretch
/// (a big-RAM machine's MMU table build keeps the disk idle for seconds),
/// hence the range up to two minutes.
pub(super) const WARP_BOOT_IDLE_PRESETS: [f64; 7] = [5.0, 10.0, 15.0, 20.0, 30.0, 60.0, 120.0];
pub(super) const CLOCK_PRESETS: [f64; 10] = [
    7.09, 14.0, 14.18, 25.0, 28.0, 33.0, 40.0, 50.0, 100.0, 200.0,
];
pub(super) const CHIP_PRESETS: [usize; 4] = [256 * 1024, 512 * 1024, 1024 * 1024, 2 * 1024 * 1024];
pub(super) const FAST_PRESETS: [usize; 9] = [
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
pub(super) const SLOW_PRESETS: [usize; 3] = [0, 256 * 1024, 512 * 1024];
/// Ramsey bank fills: 1M-4M on 256Kx4 parts, then whole 4M banks of 1Mx4.
pub(super) const MB_PRESETS: [usize; 8] = [
    0,
    1024 * 1024,
    2 * 1024 * 1024,
    3 * 1024 * 1024,
    4 * 1024 * 1024,
    8 * 1024 * 1024,
    12 * 1024 * 1024,
    16 * 1024 * 1024,
];
/// The A4000 additionally fills the $04000000-$06FFFFFF motherboard RAM
/// expansion space beyond Ramsey's four banks.
pub(super) const MB_PRESETS_A4000: [usize; 10] = [
    0,
    1024 * 1024,
    2 * 1024 * 1024,
    3 * 1024 * 1024,
    4 * 1024 * 1024,
    8 * 1024 * 1024,
    12 * 1024 * 1024,
    16 * 1024 * 1024,
    32 * 1024 * 1024,
    64 * 1024 * 1024,
];
/// CPU-slot accelerator RAM at $08000000: whatever the CPU board carries,
/// up to the whole 128M coprocessor-slot space.
pub(super) const ACCEL_PRESETS: [usize; 5] = [
    0,
    16 * 1024 * 1024,
    32 * 1024 * 1024,
    64 * 1024 * 1024,
    128 * 1024 * 1024,
];
pub(super) const Z3_PRESETS: [usize; 8] = [
    0,
    16 * 1024 * 1024,
    32 * 1024 * 1024,
    64 * 1024 * 1024,
    128 * 1024 * 1024,
    256 * 1024 * 1024,
    512 * 1024 * 1024,
    1024 * 1024 * 1024,
];
pub(super) const OVERSCANS: [Overscan; 2] = [Overscan::Tv, Overscan::Full];
pub(super) const PIXEL_ASPECTS: [PixelAspect; 2] = [PixelAspect::Tv, PixelAspect::Square];
pub(super) const TINTS: [Tint; 5] = [Tint::None, Tint::Bw, Tint::Green, Tint::Amber, Tint::Sepia];
