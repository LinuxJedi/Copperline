// SPDX-License-Identifier: GPL-3.0-or-later

//! Zorro expansion bus: an ordered autoconfig chain of boards.
//!
//! Boards appear one at a time in the Zorro II configuration space at
//! $E80000 (the A1200/A600/A500 path; this emulator has no native Zorro III
//! backplane, so Zorro III boards configure through the same window, as
//! expansion.library does on real 32-bit machines). Each board exposes the
//! nibble-encoded autoconfig ROM until the system ROM either assigns it a base
//! address or shuts it up, at which point the next unconfigured board
//! appears.
//!
//! Boards are described by a [`BoardSpec`]; built-in RAM boards come from
//! `[memory] fast`/`z3` in the config, and additional boards can be loaded
//! from TOML metadata files via `[[zorro]]` sections (the "plugin" path:
//! the autoconfig identity lives in data, not code). Only RAM-backed
//! boards are implemented today; other backings plug in via
//! [`BoardBacking`].

use crate::net::NetConfig;
use crate::wasmboard::{WasmCaps, WasmManifest};
use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::path::{Path, PathBuf};

pub const AUTOCONFIG_BASE: u64 = 0x00E8_0000;
pub const AUTOCONFIG_SIZE: u64 = 0x0001_0000;

const AUTOCONFIG_ROM_BYTES: usize = 16;
const AUTOCONFIG_PHYSICAL_BYTES: u64 = 0x80;

// er_Type fields.
const ERT_ZORROII: u8 = 0xC0;
const ERT_ZORROIII: u8 = 0x80;
const ERTF_MEMLIST: u8 = 1 << 5;
const ERTF_DIAGVALID: u8 = 1 << 4;

// er_Flags fields.
const ERFF_MEMSPACE: u8 = 1 << 7;
const ERFF_EXTENDED: u8 = 1 << 5;
const ERFF_ZORRO_III: u8 = 1 << 4;

// Configuration registers (physical offsets in the config window).
const EC_Z3_BASEADDRESS_PHYS: u64 = 0x44;
const EC_BASEADDRESS_PHYS: u64 = 0x48;
const EC_BASEADDRESS_LO_PHYS: u64 = 0x4A;
const EC_SHUTUP_PHYS: u64 = 0x4C;

/// Copperline's registered Amiga expansion manufacturer ID (dec0de
/// Consulting, 5192 / 0x1448). The same ID labels the real ROMulus
/// flash-ROM board (product 1); the emulator's virtual boards take
/// distinct product numbers under it so tools like identify.library can
/// tell them apart.
pub const COPPERLINE_MANUFACTURER_ID: u16 = 0x1448;
/// The always-present identification board guest software finds (via
/// expansion.library FindConfigDev) to detect that it is running under
/// Copperline. Product 1 is the physical ROMulus board, so the emulator's
/// product numbering starts at 2.
const PRODUCT_COPPERLINE_ID: u8 = 0x02;
const PRODUCT_FAST_RAM: u8 = 0x03;
const PRODUCT_Z3_RAM: u8 = 0x04;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
// "II"/"III" are the bus generations' proper names (roman numerals), not
// acronyms to be case-folded.
#[allow(clippy::upper_case_acronyms)]
#[derive(serde::Serialize, serde::Deserialize)]
pub enum ZorroVersion {
    II,
    III,
}

/// What sits behind the board's address space once configured.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum BoardBacking {
    /// RAM the CPU can read and write at external-bus speed.
    Ram,
    /// A functional device (registers + optional boot ROM): accesses route to
    /// `Bus::devices[usize]`, a [`crate::zorro_device::BoardDevice`], not to
    /// board RAM. The index is assigned when the board is added to the chain
    /// and matches the device attached to the bus.
    Device(usize),
}

/// The autoconfig identity and backing of one expansion board.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct BoardSpec {
    pub name: String,
    pub version: ZorroVersion,
    pub manufacturer: u16,
    pub product: u8,
    pub serial: u32,
    pub size_bytes: usize,
    pub backing: BoardBacking,
    /// Link the board space into the system free memory list (ERTF_MEMLIST).
    pub memlist: bool,
    /// Autoboot: ERTF_DIAGVALID with er_InitDiagVec pointing at the
    /// DiagArea inside the configured board space.
    pub diag_vec: Option<u16>,
}

impl BoardSpec {
    /// The built-in Zorro II fast RAM board (`[memory] fast`).
    pub fn fast_ram(size_bytes: usize) -> Self {
        Self {
            name: "fast RAM".into(),
            version: ZorroVersion::II,
            manufacturer: COPPERLINE_MANUFACTURER_ID,
            product: PRODUCT_FAST_RAM,
            serial: 0,
            size_bytes,
            backing: BoardBacking::Ram,
            memlist: true,
            diag_vec: None,
        }
    }

    /// The built-in Zorro III fast RAM board (`[memory] z3`).
    pub fn z3_ram(size_bytes: usize) -> Self {
        Self {
            name: "Zorro III RAM".into(),
            version: ZorroVersion::III,
            manufacturer: COPPERLINE_MANUFACTURER_ID,
            product: PRODUCT_Z3_RAM,
            serial: 0,
            size_bytes,
            backing: BoardBacking::Ram,
            memlist: true,
            diag_vec: None,
        }
    }

    /// The Copperline identification board: a tiny, inert Zorro II board
    /// kept on the autoconfig chain so guest software can detect the
    /// emulator by finding manufacturer [`COPPERLINE_MANUFACTURER_ID`] /
    /// product [`PRODUCT_COPPERLINE_ID`] (e.g. identify.library calling
    /// FindConfigDev). It is the smallest legal Zorro II size, is left out
    /// of the free memory list, and never autoboots, so it does not perturb
    /// the machine's memory map. Its serial number carries the running
    /// Copperline version (see [`copperline_ident_serial`]) so a tool can
    /// report the exact version, not just the emulator name.
    pub fn copperline_id() -> Self {
        Self {
            name: "Copperline".into(),
            version: ZorroVersion::II,
            manufacturer: COPPERLINE_MANUFACTURER_ID,
            product: PRODUCT_COPPERLINE_ID,
            serial: copperline_ident_serial(),
            size_bytes: 0x1_0000,
            backing: BoardBacking::Ram,
            memlist: false,
            diag_vec: None,
        }
    }

    /// The A2065 Ethernet board: Commodore West Chester (manufacturer 514),
    /// product 0x70, a 64K Zorro II window with no autoboot ROM. `slot` is the
    /// index of the matching `A2065` device in `Bus::devices`.
    pub fn a2065(slot: usize) -> Self {
        Self {
            name: "A2065 Ethernet".into(),
            version: ZorroVersion::II,
            manufacturer: 514,
            product: 0x70,
            serial: 0,
            size_bytes: 0x1_0000,
            backing: BoardBacking::Device(slot),
            memlist: false,
            diag_vec: None,
        }
    }

    /// The A2091 SCSI controller board: the DMAC supplies the autoconfig
    /// identity (Commodore West Chester, product 3) with a valid DiagArea
    /// vector pointing at the boot ROM, which appears at $2000 in the
    /// board space. `slot` is the index of the matching `A2091` device in
    /// `Bus::devices`.
    pub fn a2091(slot: usize) -> Self {
        Self {
            name: "A2091 SCSI".into(),
            version: ZorroVersion::II,
            manufacturer: 514,
            product: 3,
            serial: 0,
            size_bytes: 0x1_0000,
            backing: BoardBacking::Device(slot),
            memlist: false,
            diag_vec: Some(0x2000),
        }
    }

    fn validate(&self) -> Result<()> {
        match self.version {
            ZorroVersion::II => {
                if zorro_ii_size_code(self.size_bytes).is_none() {
                    bail!(
                        "board {:?}: {} bytes is not a Zorro II autoconfig size \
                         (64K, 128K, 256K, 512K, 1M, 2M, 4M, or 8M)",
                        self.name,
                        self.size_bytes
                    );
                }
            }
            ZorroVersion::III => {
                if zorro_iii_size_bits(self.size_bytes).is_none() {
                    bail!(
                        "board {:?}: {} bytes is not a Zorro III autoconfig size \
                         (a power of two from 64K to 1G)",
                        self.name,
                        self.size_bytes
                    );
                }
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
enum BoardState {
    Unconfigured,
    Configured { base: u32 },
    ShutUp,
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct Board {
    spec: BoardSpec,
    state: BoardState,
    ram: Vec<u8>,
}

/// The autoconfig chain plus the configured boards' address windows.
#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct ZorroChain {
    boards: Vec<Board>,
    /// Configured RAM windows, rebuilt whenever a board's state changes:
    /// (base, len, board index). Kept flat so the CPU's per-access lookup
    /// is a scan over at most a handful of entries.
    regions: Vec<(u32, u32, usize)>,
    /// Configured device-board windows (base, len, board index); accesses
    /// route to the device on the bus instead of board RAM.
    device_regions: Vec<(u32, u32, usize)>,
}

impl ZorroChain {
    pub fn add_board(&mut self, spec: BoardSpec) -> Result<()> {
        spec.validate()?;
        let ram = match spec.backing {
            BoardBacking::Ram => vec![0u8; spec.size_bytes],
            BoardBacking::Device(_) => Vec::new(),
        };
        self.boards.push(Board {
            spec,
            state: BoardState::Unconfigured,
            ram,
        });
        Ok(())
    }

    /// Add a board already configured at a fixed base, bypassing the
    /// autoconfig handshake (for fixtures and non-autoconfig mappings).
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn add_board_configured_at(&mut self, spec: BoardSpec, base: u32) -> Result<()> {
        self.add_board(spec)?;
        let idx = self.boards.len() - 1;
        self.boards[idx].state = BoardState::Configured { base };
        self.rebuild_regions();
        Ok(())
    }

    /// Return all boards to unconfigured with zeroed RAM (cold power-on).
    pub fn power_on_reset(&mut self) {
        for board in &mut self.boards {
            board.state = BoardState::Unconfigured;
            board.ram.fill(0);
        }
        self.regions.clear();
        self.device_regions.clear();
    }

    fn current(&self) -> Option<usize> {
        self.boards
            .iter()
            .position(|b| b.state == BoardState::Unconfigured)
    }

    fn rebuild_regions(&mut self) {
        self.regions.clear();
        self.device_regions.clear();
        for (idx, board) in self.boards.iter().enumerate() {
            if let BoardState::Configured { base } = board.state {
                match board.spec.backing {
                    BoardBacking::Ram => {
                        if !board.ram.is_empty() {
                            self.regions.push((base, board.ram.len() as u32, idx));
                        }
                    }
                    BoardBacking::Device(_) => {
                        self.device_regions
                            .push((base, board.spec.size_bytes as u32, idx));
                    }
                }
            }
        }
    }

    // ----- configured RAM windows ----------------------------------------

    /// Locate `addr..addr+size` inside a configured board's RAM window.
    /// Returns the board index and byte offset.
    pub fn region_at(&self, addr: u32, size: usize) -> Option<(usize, usize)> {
        for &(base, len, idx) in &self.regions {
            let off = addr.wrapping_sub(base);
            if u64::from(off) + size as u64 <= u64::from(len) {
                return Some((idx, off as usize));
            }
        }
        None
    }

    pub fn board_ram(&self, idx: usize) -> &[u8] {
        &self.boards[idx].ram
    }

    pub fn board_ram_mut(&mut self, idx: usize) -> &mut [u8] {
        &mut self.boards[idx].ram
    }

    /// Locate `addr..addr+size` inside a configured device board's window,
    /// returning the backing kind and byte offset.
    pub fn device_region_at(&self, addr: u32, size: usize) -> Option<(BoardBacking, u32)> {
        for &(base, len, idx) in &self.device_regions {
            let off = addr.wrapping_sub(base);
            if u64::from(off) + size as u64 <= u64::from(len) {
                return Some((self.boards[idx].spec.backing, off));
            }
        }
        None
    }

    // ----- autoconfig window ----------------------------------------------

    pub fn config_read(&self, addr: u64, size: usize) -> u64 {
        let Some(idx) = self.current() else {
            return floating_bus(size);
        };
        let off = addr.wrapping_sub(AUTOCONFIG_BASE) & (AUTOCONFIG_SIZE - 1);
        match size {
            1 => self.config_read_byte(idx, off) as u64,
            2 => {
                let hi = self.config_read_byte(idx, off) as u16;
                let lo = self.config_read_byte(idx, off + 1) as u16;
                ((hi << 8) | lo) as u64
            }
            4 => {
                let b0 = self.config_read_byte(idx, off) as u32;
                let b1 = self.config_read_byte(idx, off + 1) as u32;
                let b2 = self.config_read_byte(idx, off + 2) as u32;
                let b3 = self.config_read_byte(idx, off + 3) as u32;
                ((b0 << 24) | (b1 << 16) | (b2 << 8) | b3) as u64
            }
            _ => floating_bus(size),
        }
    }

    pub fn config_write(&mut self, addr: u64, size: usize, val: u64) {
        let Some(idx) = self.current() else {
            return;
        };
        let off = addr.wrapping_sub(AUTOCONFIG_BASE) & (AUTOCONFIG_SIZE - 1);
        let data_byte = match size {
            1 => (val & 0xFF) as u8,
            2 | 4 => ((val >> ((size - 1) * 8)) & 0xFF) as u8,
            _ => return,
        };
        let board = &mut self.boards[idx];
        match off & 0x7E {
            EC_Z3_BASEADDRESS_PHYS => {
                // Zorro III boards take a 16-bit write of the base address
                // high word here, which both assigns and configures.
                if board.spec.version == ZorroVersion::III && size >= 2 {
                    let base = ((val & 0xFFFF) as u32) << 16;
                    board.state = BoardState::Configured { base };
                    log::info!(
                        "zorro III board {:?} autoconfigured at {:#010X}",
                        board.spec.name,
                        base
                    );
                    self.rebuild_regions();
                }
            }
            EC_BASEADDRESS_PHYS => {
                // Zorro II boards take the base high byte here, which both
                // assigns and configures.
                if board.spec.version == ZorroVersion::II {
                    let base = u32::from(data_byte) << 16;
                    board.state = BoardState::Configured { base };
                    log::info!(
                        "zorro II board {:?} autoconfigured at {:#010X}",
                        board.spec.name,
                        base
                    );
                    self.rebuild_regions();
                }
            }
            EC_BASEADDRESS_LO_PHYS => {
                // The system ROM writes the low nibble here before the full byte
                // goes to ec_BaseAddress. Not needed for memory boards, but
                // accepting it keeps the register sequence faithful.
            }
            EC_SHUTUP_PHYS => {
                board.state = BoardState::ShutUp;
                log::debug!("zorro board {:?} shut up", board.spec.name);
                self.rebuild_regions();
            }
            _ => {}
        }
    }

    fn config_read_byte(&self, idx: usize, off: u64) -> u8 {
        if off >= AUTOCONFIG_PHYSICAL_BYTES || off & 1 != 0 {
            return 0xFF;
        }
        let Some(logical) = self.config_logical_byte(idx, (off / 4) as usize) else {
            return 0xFF;
        };
        let physical = if off / 4 == 0 { logical } else { !logical };
        let nibble = if off & 2 == 0 {
            physical >> 4
        } else {
            physical & 0x0F
        };
        nibble << 4
    }

    fn config_logical_byte(&self, idx: usize, byte: usize) -> Option<u8> {
        if byte >= AUTOCONFIG_ROM_BYTES {
            return None;
        }
        let spec = &self.boards[idx].spec;
        let mut rom = [0u8; AUTOCONFIG_ROM_BYTES];
        let memlist = if spec.memlist { ERTF_MEMLIST } else { 0 };
        // I/O-style device boards do not claim the memory space flag.
        let memspace = if spec.backing == BoardBacking::Ram {
            ERFF_MEMSPACE
        } else {
            0
        };
        match spec.version {
            ZorroVersion::II => {
                rom[0] = ERT_ZORROII | memlist | zorro_ii_size_code(spec.size_bytes)?;
                rom[2] = memspace;
            }
            ZorroVersion::III => {
                let (code, extended) = zorro_iii_size_bits(spec.size_bytes)?;
                rom[0] = ERT_ZORROIII | memlist | code;
                rom[2] = memspace | ERFF_ZORRO_III | if extended { ERFF_EXTENDED } else { 0 };
            }
        }
        if let Some(vec) = spec.diag_vec {
            rom[0] |= ERTF_DIAGVALID;
            rom[10..12].copy_from_slice(&vec.to_be_bytes());
        }
        rom[1] = spec.product;
        rom[4..6].copy_from_slice(&spec.manufacturer.to_be_bytes());
        rom[6..10].copy_from_slice(&spec.serial.to_be_bytes());
        Some(rom[byte])
    }
}

/// Zorro II er_Type size code for a board space size, if representable.
pub fn zorro_ii_size_code(bytes: usize) -> Option<u8> {
    match bytes {
        0x0080_0000 => Some(0),
        0x0001_0000 => Some(1),
        0x0002_0000 => Some(2),
        0x0004_0000 => Some(3),
        0x0008_0000 => Some(4),
        0x0010_0000 => Some(5),
        0x0020_0000 => Some(6),
        0x0040_0000 => Some(7),
        _ => None,
    }
}

/// Zorro III er_Type size code and the er_Flags EXTENDED bit. Sizes up to
/// 8M use the Zorro II codes; 16M-1G use the extended encoding.
pub fn zorro_iii_size_bits(bytes: usize) -> Option<(u8, bool)> {
    if let Some(code) = zorro_ii_size_code(bytes) {
        return Some((code, false));
    }
    let code = match bytes {
        0x0100_0000 => 0, // 16M
        0x0200_0000 => 1, // 32M
        0x0400_0000 => 2, // 64M
        0x0800_0000 => 3, // 128M
        0x1000_0000 => 4, // 256M
        0x2000_0000 => 5, // 512M
        0x4000_0000 => 6, // 1G
        _ => return None,
    };
    Some((code, true))
}

fn floating_bus(size: usize) -> u64 {
    match size {
        1 => 0xFF,
        2 => 0xFFFF,
        4 => 0xFFFF_FFFF,
        _ => 0,
    }
}

// ----- metadata files -----------------------------------------------------

/// On-disk board metadata (`[[zorro]] metadata = "board.toml"`):
///
/// ```toml
/// name = "MegaRAM"
/// zorro = 3            # 2 or 3
/// type = "ram"         # "ram" or "wasm"
/// size = "64M"
/// manufacturer = 0x07DB
/// product = 0x20
/// serial = 0           # optional
/// memlist = true       # optional; defaults true for "ram", false for "wasm"
///
/// # For type = "wasm" (a plugin board), additionally:
/// wasm = "board.wasm"  # module path, relative to this metadata file
/// dma = true           # optional capabilities (default false)
/// int2 = true
/// int6 = false
/// ```
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawBoardMeta {
    name: String,
    zorro: u8,
    #[serde(rename = "type")]
    backing: String,
    size: String,
    manufacturer: u32,
    product: u16,
    serial: Option<u32>,
    memlist: Option<bool>,
    // WASM plugin boards (type = "wasm"):
    wasm: Option<String>,
    dma: Option<bool>,
    int2: Option<bool>,
    int6: Option<bool>,
    /// Host network backend ("none"/"loopback"); presence grants the `net`
    /// capability (the net_send/net_recv imports).
    net: Option<String>,
}

/// A board parsed from a TOML metadata file: a RAM board (fully described by
/// its autoconfig identity) or a WASM plugin board (identity plus the module
/// path and capabilities, instantiated at machine-build time).
#[derive(Debug)]
pub enum LoadedZorroBoard {
    Ram(BoardSpec),
    Wasm {
        /// Autoconfig identity; `backing` is a `Device` placeholder reassigned
        /// to the real slot index when the board is attached to the bus.
        spec: BoardSpec,
        wasm_path: PathBuf,
        manifest: WasmManifest,
    },
}

/// Load a board description from a TOML metadata file.
pub fn load_board_metadata(path: &Path) -> Result<LoadedZorroBoard> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading zorro board metadata {}", path.display()))?;
    let raw: RawBoardMeta = toml::from_str(&text)
        .with_context(|| format!("parsing zorro board metadata {}", path.display()))?;
    let version = match raw.zorro {
        2 => ZorroVersion::II,
        3 => ZorroVersion::III,
        other => bail!(
            "{}: zorro = {} is not supported (expected 2 or 3)",
            path.display(),
            other
        ),
    };
    if raw.manufacturer > 0xFFFF {
        bail!("{}: manufacturer must be a 16-bit ID", path.display());
    }
    if raw.product > 0xFF {
        bail!("{}: product must be an 8-bit ID", path.display());
    }
    let size_bytes = parse_board_size(&raw.size)
        .with_context(|| format!("{}: bad size {:?}", path.display(), raw.size))?;
    let kind = raw.backing.trim().to_ascii_lowercase();
    match kind.as_str() {
        "ram" => {
            let spec = BoardSpec {
                name: raw.name,
                version,
                manufacturer: raw.manufacturer as u16,
                product: raw.product as u8,
                serial: raw.serial.unwrap_or(0),
                size_bytes,
                backing: BoardBacking::Ram,
                memlist: raw.memlist.unwrap_or(true),
                diag_vec: None,
            };
            spec.validate()
                .with_context(|| format!("{}: invalid board", path.display()))?;
            Ok(LoadedZorroBoard::Ram(spec))
        }
        "wasm" => {
            let Some(wasm) = raw.wasm else {
                bail!(
                    "{}: type = \"wasm\" needs a `wasm = \"module.wasm\"` path",
                    path.display()
                );
            };
            // Module path resolves relative to the metadata file's directory.
            let wasm_path = match path.parent() {
                Some(dir) => dir.join(&wasm),
                None => PathBuf::from(&wasm),
            };
            // The real slot index is assigned when the device is attached.
            let spec = BoardSpec {
                name: raw.name,
                version,
                manufacturer: raw.manufacturer as u16,
                product: raw.product as u8,
                serial: raw.serial.unwrap_or(0),
                size_bytes,
                backing: BoardBacking::Device(0),
                memlist: raw.memlist.unwrap_or(false),
                diag_vec: None,
            };
            spec.validate()
                .with_context(|| format!("{}: invalid board", path.display()))?;
            let net = match &raw.net {
                Some(s) => crate::net::parse_net_config(s).ok_or_else(|| {
                    anyhow::anyhow!(
                        "{}: net = {:?} is not a known backend (expected \"none\" or \"loopback\")",
                        path.display(),
                        s
                    )
                })?,
                None => NetConfig::None,
            };
            let manifest = WasmManifest {
                name: spec.name.clone(),
                caps: WasmCaps {
                    dma: raw.dma.unwrap_or(false),
                    int2: raw.int2.unwrap_or(false),
                    int6: raw.int6.unwrap_or(false),
                    net: raw.net.is_some(),
                },
                net,
            };
            Ok(LoadedZorroBoard::Wasm {
                spec,
                wasm_path,
                manifest,
            })
        }
        other => bail!(
            "{}: type = {:?} is not supported (expected \"ram\" or \"wasm\")",
            path.display(),
            other
        ),
    }
}

fn parse_board_size(s: &str) -> Result<usize> {
    let raw = s.trim();
    let split = raw.find(|c: char| !c.is_ascii_digit()).unwrap_or(raw.len());
    let (num_str, unit_str) = raw.split_at(split);
    let n: u64 = num_str.parse().context("bad number")?;
    let unit = unit_str.trim().to_ascii_uppercase().replace("IB", "B");
    let bytes = match unit.as_str() {
        "" | "B" => n,
        "K" | "KB" => n * 1024,
        "M" | "MB" => n * 1024 * 1024,
        "G" | "GB" => n * 1024 * 1024 * 1024,
        _ => bail!("unknown unit {:?}", unit_str),
    };
    Ok(bytes as usize)
}

/// Pack a `major.minor.patch` version string into a 32-bit autoconfig
/// serial number: byte 2 holds major, byte 1 minor, byte 0 patch. Missing
/// or non-numeric components (e.g. a `-dev` pre-release suffix) count as 0.
fn pack_version_serial(version: &str) -> u32 {
    let mut parts = version.split('.');
    let major: u32 = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
    let minor: u32 = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
    let patch: u32 = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
    (major << 16) | (minor << 8) | patch
}

/// The running Copperline version packed into a board serial number, so the
/// identification board ([`BoardSpec::copperline_id`]) can advertise it.
fn copperline_ident_serial() -> u32 {
    pack_version_serial(env!("CARGO_PKG_VERSION"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chain_with(specs: Vec<BoardSpec>) -> ZorroChain {
        let mut chain = ZorroChain::default();
        for spec in specs {
            chain.add_board(spec).unwrap();
        }
        chain
    }

    #[test]
    fn zorro_ii_size_codes_match_autoconfig_slots() {
        assert_eq!(zorro_ii_size_code(64 * 1024), Some(1));
        assert_eq!(zorro_ii_size_code(512 * 1024), Some(4));
        assert_eq!(zorro_ii_size_code(8 * 1024 * 1024), Some(0));
        assert_eq!(zorro_ii_size_code(768 * 1024), None);
    }

    #[test]
    fn zorro_iii_sizes_cover_extended_range() {
        assert_eq!(zorro_iii_size_bits(2 * 1024 * 1024), Some((6, false)));
        assert_eq!(zorro_iii_size_bits(16 * 1024 * 1024), Some((0, true)));
        assert_eq!(zorro_iii_size_bits(1024 * 1024 * 1024), Some((6, true)));
        assert_eq!(zorro_iii_size_bits(3 * 1024 * 1024), None);
    }

    #[test]
    fn fast_ram_autoconfig_rom_is_nibble_encoded() {
        let chain = chain_with(vec![BoardSpec::fast_ram(512 * 1024)]);

        // er_Type is not inverted. 512K => size code 4, with Zorro II
        // and MEMLIST bits set: 0xC0 | 0x20 | 0x04 = 0xE4.
        assert_eq!(chain.config_read(AUTOCONFIG_BASE, 1), 0xE0);
        assert_eq!(chain.config_read(AUTOCONFIG_BASE + 2, 1), 0x40);

        // er_Product is inverted before being exposed on the physical
        // nibbles: product 0x03 appears as 0xFC.
        assert_eq!(chain.config_read(AUTOCONFIG_BASE + 4, 1), 0xF0);
        assert_eq!(chain.config_read(AUTOCONFIG_BASE + 6, 1), 0xC0);
    }

    #[test]
    fn copperline_id_board_advertises_registered_manufacturer() {
        let spec = BoardSpec::copperline_id();
        // dec0de Consulting's registered manufacturer ID, with a product
        // number distinct from the physical ROMulus board (product 1).
        assert_eq!(spec.manufacturer, COPPERLINE_MANUFACTURER_ID);
        assert_eq!(spec.manufacturer, 5192);
        assert_eq!(spec.product, PRODUCT_COPPERLINE_ID);
        // Inert: smallest Zorro II size, out of the free memory list, and
        // no autoboot, so it does not perturb the machine's memory map.
        assert_eq!(spec.version, ZorroVersion::II);
        assert_eq!(spec.size_bytes, 64 * 1024);
        assert!(!spec.memlist);
        assert!(spec.diag_vec.is_none());
        // Serial advertises the running Copperline version.
        assert_eq!(spec.serial, copperline_ident_serial());
        spec.validate().unwrap();
    }

    #[test]
    fn version_serial_packs_major_minor_patch() {
        assert_eq!(pack_version_serial("0.6.0"), 0x0000_0600);
        assert_eq!(pack_version_serial("1.2.3"), 0x0001_0203);
        assert_eq!(pack_version_serial("0.5"), 0x0000_0500);
        assert_eq!(pack_version_serial("12.34.56"), (12 << 16) | (34 << 8) | 56);
        // A pre-release suffix on the patch component counts as 0.
        assert_eq!(pack_version_serial("0.6.0-dev"), 0x0000_0600);
    }

    #[test]
    fn z2_base_write_configures_board_and_advances_chain() {
        let mut chain = chain_with(vec![
            BoardSpec::fast_ram(512 * 1024),
            BoardSpec::z3_ram(16 * 1024 * 1024),
        ]);

        chain.config_write(AUTOCONFIG_BASE + EC_BASEADDRESS_PHYS, 2, 0x2000);

        // The fast RAM board is now configured at $200000 and its RAM
        // window responds there.
        assert_eq!(chain.region_at(0x0020_0000, 2), Some((0, 0)));
        assert_eq!(chain.region_at(0x0027_FFFE, 2), Some((0, 0x7_FFFE)));
        assert_eq!(chain.region_at(0x0028_0000, 2), None);

        // The next board (Zorro III) is now in the config window:
        // er_Type = ERT_ZORROIII | ERTF_MEMLIST | 0 (16M extended).
        assert_eq!(chain.config_read(AUTOCONFIG_BASE, 1), 0xA0);
        assert_eq!(chain.config_read(AUTOCONFIG_BASE + 2, 1), 0x00);
    }

    #[test]
    fn z3_board_configures_via_word_write_to_44() {
        let mut chain = chain_with(vec![BoardSpec::z3_ram(16 * 1024 * 1024)]);

        // er_Flags: MEMSPACE | ZORRO_III | EXTENDED = 0xB0, inverted to
        // 0x4F on the physical nibbles.
        assert_eq!(chain.config_read(AUTOCONFIG_BASE + 8, 1), 0x40);
        assert_eq!(chain.config_read(AUTOCONFIG_BASE + 10, 1), 0xF0);

        chain.config_write(AUTOCONFIG_BASE + EC_Z3_BASEADDRESS_PHYS, 2, 0x4000);

        assert_eq!(chain.region_at(0x4000_0000, 4), Some((0, 0)));
        assert_eq!(chain.region_at(0x40FF_FFFC, 4), Some((0, 0x00FF_FFFC)));
        assert_eq!(chain.region_at(0x4100_0000, 4), None);
        // Chain exhausted: config space floats.
        assert_eq!(chain.config_read(AUTOCONFIG_BASE, 1), 0xFF);
    }

    #[test]
    fn shutup_disables_board_without_mapping_it() {
        let mut chain = chain_with(vec![BoardSpec::fast_ram(512 * 1024)]);

        chain.config_write(AUTOCONFIG_BASE + EC_SHUTUP_PHYS, 1, 0);

        assert_eq!(chain.region_at(0x0020_0000, 2), None);
        assert_eq!(chain.config_read(AUTOCONFIG_BASE, 1), 0xFF);
    }

    #[test]
    fn power_on_reset_unconfigures_and_clears_ram() {
        let mut chain = chain_with(vec![BoardSpec::fast_ram(512 * 1024)]);
        chain.config_write(AUTOCONFIG_BASE + EC_BASEADDRESS_PHYS, 2, 0x2000);
        let (idx, off) = chain.region_at(0x0020_0000, 1).unwrap();
        chain.board_ram_mut(idx)[off] = 0xAA;

        chain.power_on_reset();

        assert_eq!(chain.region_at(0x0020_0000, 1), None);
        assert_eq!(chain.config_read(AUTOCONFIG_BASE, 1), 0xE0);
        assert_eq!(chain.board_ram(0)[0], 0);
    }

    #[test]
    fn board_metadata_file_round_trips() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!(
            "copperline-zorro-meta-{}-{}.toml",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::write(
            &path,
            r#"
            name = "MegaRAM"
            zorro = 3
            type = "ram"
            size = "64M"
            manufacturer = 2011
            product = 32
            "#,
        )
        .unwrap();
        let loaded = load_board_metadata(&path).unwrap();
        let _ = std::fs::remove_file(&path);

        let LoadedZorroBoard::Ram(spec) = loaded else {
            panic!("expected a RAM board");
        };
        assert_eq!(spec.name, "MegaRAM");
        assert_eq!(spec.version, ZorroVersion::III);
        assert_eq!(spec.backing, BoardBacking::Ram);
        assert_eq!(spec.size_bytes, 64 * 1024 * 1024);
        assert_eq!(spec.manufacturer, 2011);
        assert_eq!(spec.product, 32);
        assert!(spec.memlist);
    }

    #[test]
    fn wasm_board_metadata_parses_path_and_caps() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!(
            "copperline-zorro-wasm-{}-{}.toml",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::write(
            &path,
            r#"
            name = "Test NIC"
            zorro = 2
            type = "wasm"
            size = "64K"
            manufacturer = 5192
            product = 16
            wasm = "nic.wasm"
            dma = true
            int2 = true
            "#,
        )
        .unwrap();
        let loaded = load_board_metadata(&path).unwrap();
        let _ = std::fs::remove_file(&path);

        let LoadedZorroBoard::Wasm {
            spec,
            wasm_path,
            manifest,
        } = loaded
        else {
            panic!("expected a WASM board");
        };
        assert_eq!(spec.name, "Test NIC");
        assert_eq!(spec.version, ZorroVersion::II);
        assert!(matches!(spec.backing, BoardBacking::Device(_)));
        assert!(!spec.memlist);
        // Module path is resolved next to the metadata file.
        assert_eq!(wasm_path, dir.join("nic.wasm"));
        assert_eq!(manifest.name, "Test NIC");
        assert!(manifest.caps.dma);
        assert!(manifest.caps.int2);
        assert!(!manifest.caps.int6);
    }

    #[test]
    fn bad_metadata_is_rejected() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!(
            "copperline-zorro-badmeta-{}.toml",
            std::process::id()
        ));
        std::fs::write(
            &path,
            r#"
            name = "Weird"
            zorro = 4
            type = "ram"
            size = "64M"
            manufacturer = 2011
            product = 32
            "#,
        )
        .unwrap();
        let err = load_board_metadata(&path).unwrap_err();
        let _ = std::fs::remove_file(&path);
        assert!(err.to_string().contains("zorro = 4"), "{err:#}");
    }

    #[test]
    fn serial_number_is_exposed_in_inverted_nibbles() {
        let mut spec = BoardSpec::fast_ram(512 * 1024);
        spec.serial = 0x1234_5678;
        let chain = chain_with(vec![spec]);

        // er_SerialNumber occupies logical bytes 6..10; every byte after
        // er_Type reads back inverted, one nibble per physical word.
        let mut serial = 0u32;
        for byte in 0..4u64 {
            let off = AUTOCONFIG_BASE + (6 + byte) * 4;
            let hi = (chain.config_read(off, 1) as u8) >> 4;
            let lo = (chain.config_read(off + 2, 1) as u8) >> 4;
            serial = (serial << 8) | u32::from(!((hi << 4) | lo));
        }
        assert_eq!(serial, 0x1234_5678);
    }

    #[test]
    fn diag_vector_sets_diagvalid_and_init_diag_vec() {
        let chain = chain_with(vec![BoardSpec::a2091(0)]);

        // er_Type carries ERTF_DIAGVALID (autoboot ROM present)...
        let hi = (chain.config_read(AUTOCONFIG_BASE, 1) as u8) >> 4;
        let lo = (chain.config_read(AUTOCONFIG_BASE + 2, 1) as u8) >> 4;
        let er_type = (hi << 4) | lo;
        assert_ne!(er_type & ERTF_DIAGVALID, 0);

        // ...and er_InitDiagVec (logical bytes 10..12) holds the DiagArea
        // offset inside the board space, inverted on the wire.
        let mut vec = 0u16;
        for byte in 0..2u64 {
            let off = AUTOCONFIG_BASE + (10 + byte) * 4;
            let hi = (chain.config_read(off, 1) as u8) >> 4;
            let lo = (chain.config_read(off + 2, 1) as u8) >> 4;
            vec = (vec << 8) | u16::from(!((hi << 4) | lo));
        }
        assert_eq!(vec, 0x2000);
    }

    #[test]
    fn word_and_long_config_reads_assemble_byte_lanes() {
        let chain = chain_with(vec![BoardSpec::fast_ram(512 * 1024)]);

        let b0 = chain.config_read(AUTOCONFIG_BASE, 1);
        let b1 = chain.config_read(AUTOCONFIG_BASE + 1, 1);
        let b2 = chain.config_read(AUTOCONFIG_BASE + 2, 1);
        let b3 = chain.config_read(AUTOCONFIG_BASE + 3, 1);
        assert_eq!(chain.config_read(AUTOCONFIG_BASE, 2), (b0 << 8) | b1);
        assert_eq!(
            chain.config_read(AUTOCONFIG_BASE, 4),
            (b0 << 24) | (b1 << 16) | (b2 << 8) | b3
        );
        // Odd physical bytes float high, as on the real bus.
        assert_eq!(b1, 0xFF);
        assert_eq!(b3, 0xFF);
    }

    #[test]
    fn low_nibble_then_high_byte_base_write_sequence_configures() {
        // The system ROM writes ec_BaseAddress low nibble ($4A) before the
        // high byte ($48); the sequence must configure exactly once.
        let mut chain = chain_with(vec![BoardSpec::fast_ram(512 * 1024)]);
        chain.config_write(AUTOCONFIG_BASE + EC_BASEADDRESS_LO_PHYS, 1, 0x00);
        assert_eq!(chain.region_at(0x0020_0000, 2), None, "not yet configured");
        chain.config_write(AUTOCONFIG_BASE + EC_BASEADDRESS_PHYS, 1, 0x20);
        assert_eq!(chain.region_at(0x0020_0000, 2), Some((0, 0)));
    }

    #[test]
    fn three_board_chain_configures_in_order() {
        let mut chain = chain_with(vec![
            BoardSpec::fast_ram(2 * 1024 * 1024),
            BoardSpec::a2091(0),
            BoardSpec::z3_ram(16 * 1024 * 1024),
        ]);

        // Board 1: Zorro II RAM at $200000.
        chain.config_write(AUTOCONFIG_BASE + EC_BASEADDRESS_PHYS, 1, 0x20);
        assert_eq!(chain.region_at(0x0020_0000, 2), Some((0, 0)));

        // Board 2: the A2091 device window at $E90000.
        chain.config_write(AUTOCONFIG_BASE + EC_BASEADDRESS_PHYS, 1, 0xE9);
        assert_eq!(
            chain.device_region_at(0x00E9_0000, 2),
            Some((BoardBacking::Device(0), 0))
        );

        // Board 3: Zorro III RAM via the 16-bit write to $44.
        chain.config_write(AUTOCONFIG_BASE + EC_Z3_BASEADDRESS_PHYS, 2, 0x4000);
        assert_eq!(chain.region_at(0x4000_0000, 2), Some((2, 0)));

        // The chain is exhausted: the config window floats.
        assert_eq!(chain.config_read(AUTOCONFIG_BASE, 1), 0xFF);
    }
}
