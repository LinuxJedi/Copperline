// SPDX-License-Identifier: GPL-3.0-or-later

//! Standard Amiga DD floppy/ADF support.
//!
//! The controller presents decoded 901,120 byte ADF images as the raw
//! AmigaDOS MFM track stream Paula would DMA. Paula does not decode
//! sectors in hardware; ROM/trackdisk drivers read the MFM words into
//! chip RAM and decode them in software.

use crate::chipset::paula::PAULA_CLOCK_HZ;
use crate::config::{FloppyConfig, FloppyDriveConfig};
use crate::gzip;
use anyhow::{bail, ensure, Context, Result};
use formats::{
    FloppyImage, FloppyImageBacking, FloppyImageData, FloppyTrackImage, GZIP_IMAGE_LIMIT,
};
use log::{debug, warn};
// The bridge's retry path traces, and its media reporting is worth an info
// line; both are compiled out with the feature.
#[cfg(feature = "fluxbridge")]
use log::{info, trace};
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

pub const CYLINDERS: usize = 80;
pub const SIDES: usize = 2;
pub const SECTORS_PER_TRACK: usize = 11;
pub const BYTES_PER_SECTOR: usize = 512;
pub const ADF_SIZE: usize = CYLINDERS * SIDES * SECTORS_PER_TRACK * BYTES_PER_SECTOR;
// IPF and SCP represent cylinders 0..=83, including unformatted slots.
const MAX_EXTENDED_TRACKS: usize = 2 * 84;
const SCP_TRACKS: usize = 168;

const CIAA_DSKCHANGE: u8 = 1 << 2;
const CIAA_DSKPROT: u8 = 1 << 3;
const CIAA_DSKTRACK0: u8 = 1 << 4;
const CIAA_DSKRDY: u8 = 1 << 5;

const CIAB_DSKSTEP: u8 = 1 << 0;
const CIAB_DSKDIREC: u8 = 1 << 1;
const CIAB_DSKSIDE: u8 = 1 << 2;
const CIAB_DSKSEL0: u8 = 1 << 3;
const CIAB_DSKSEL_MASKS: [u8; 4] = [CIAB_DSKSEL0, 1 << 4, 1 << 5, 1 << 6];
const CIAB_DSKMOTOR: u8 = 1 << 7;

const DSKLEN_DMAEN: u16 = 1 << 15;
const DSKLEN_WRITE: u16 = 1 << 14;
const DSKLEN_MASK: u16 = 0x3FFF;

const DSKBYT: u16 = 1 << 15;
const DMAON: u16 = 1 << 14;
const DISKWRITE: u16 = 1 << 13;
const WORDEQUAL: u16 = 1 << 12;

const ADK_WORDSYNC: u16 = 1 << 10;
const ADK_MSBSYNC: u16 = 1 << 9;

const DMACON_DISK: u16 = 1 << 4;
const DMACON_DMAEN: u16 = 1 << 9;

const MOTOR_READY_CCK: u32 = PAULA_CLOCK_HZ / 4;
/// Whether `COPPERLINE_DIAG_FLUXBRIDGE` asked for the physical drive's own
/// running commentary. Snapshotted, like every other diagnostic switch.
#[cfg(feature = "fluxbridge")]
fn bridge_diag() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| crate::envcfg::flag("COPPERLINE_DIAG_FLUXBRIDGE"))
}

/// How long to leave a bridged drive alone after it says a track is not ready
/// yet. The physical capture takes a revolution either way, so this only
/// decides how often we ask -- see `ensure_track`.
#[cfg(feature = "fluxbridge")]
const BRIDGE_POLL_INTERVAL_CCK: u64 = (PAULA_CLOCK_HZ / 1_000) as u64;

/// Bits a capture in flight must hold before it is served as it grows.
///
/// The emulated head reads the growing revolution at the platter's real pace
/// while the capture grows at the platter's real pace, so a head started this
/// far behind the growth edge stays behind it. Forty milliseconds of track:
/// five times the growth's publish granularity.
#[cfg(feature = "fluxbridge")]
const BRIDGE_PARTIAL_MIN_BITS: usize = 20_000;
const DISK_STATUS_SETTLE_CCK: u32 = PAULA_CLOCK_HZ / 1_000;
const INDEX_PULSE_CCK: u32 = PAULA_CLOCK_HZ / 250;
const INDEX_FLAG_SYNC_CCK: u32 = 1;
// HRM lists 3 ms step spacing and 18 ms direction-reversal spacing as drive
// programming requirements. The mechanism itself is faster but not instant:
// pulses spaced closer than ~40 us do not move the head at all (the stepper
// cannot accept them; vAmigaTS Drive/step3/step4 pin this against real-drive
// photos), and a direction reversal needs the same gap after the last step
// in the opposite direction. Pulses at or above that floor move the head
// (and the /TRK0 sensor) immediately, so recalibration -- which polls /TRK0
// between fast-but-legal step pulses -- never stalls (see cia_a_status_bits).
// What real hardware adds on top is a read-after-seek data-settle: while the head is physically traversing, the cells under it are
// not the destination track's data, so a trackloader that reads immediately
// after seeking (rather than waiting trackdisk's 15 ms) catches garbage until
// the head arrives, costing it up to a rotation of latency. We model that by
// holding off VALID read-data recovery for the head-move time after each step
// (longer on a direction reversal) while the platter keeps spinning, so the
// post-seek read resumes at a rotated position. Position sense (/TRK0) and
// motor/RDY are unaffected, so seeking and recalibration stay instant.
const SEEK_STEP_SETTLE_CCK: u32 = PAULA_CLOCK_HZ / 1_000 * 3; // ~3 ms per step
                                                              // Minimum spacing between step pulses the mechanism accepts (~40 us; the
                                                              // A1010 stepper cannot follow faster pulse trains), and the extra gap
                                                              // required after a step in the opposite direction. 140 cck = 39.5 us,
                                                              // calibrated so vAmigaTS Drive/step2 (whose CIA-paced pulse loop lands on
                                                              // 140 cck spacing under the E-clock-synced access timing) steps on every
                                                              // pulse while step3's one-iteration-shorter loop (135 cck, with occasional
                                                              // E-phase stretches to 140) only moves the head part of the time -- the
                                                              // distinction the two tests exist to probe.
const MIN_STEP_PULSE_CCK: u64 = 140;
const SEEK_REVERSAL_SETTLE_CCK: u32 = PAULA_CLOCK_HZ / 1_000 * 18; // ~18 ms on reversal
                                                                   // 300 RPM.
const ROTATION_HZ: u32 = 5;
// Turbo mode defers the burst completion of a freshly armed DMA by two
// scanlines of emulated time (the same deferral WinUAE/FS-UAE apply to
// their instant path): loaders commonly write DSKLEN and only then clear
// stale INTREQ bits before enabling the DSKBLK interrupt, and a completion
// raised in the very next colour clock would be eaten by that clear.
const TURBO_DMA_GRACE_CCK: u32 = 454;

/// Drive data-rate percentages the `[floppy] speed` option accepts, besides
/// 0 (turbo). Whole multiples of real speed keep the cck -> bitcell scaling
/// exact, so the entire read/write pipeline (shifter, sync, DSKBYTR, index)
/// stays bit-identical to real speed, only compressed in time.
pub const SUPPORTED_SPEED_PERCENTS: [u16; 4] = [100, 200, 400, 800];
/// `[floppy] speed` value selecting turbo mode.
pub const SPEED_TURBO: u16 = 0;

#[cfg(feature = "fluxbridge")]
fn default_bridge_speed_percent() -> u16 {
    100
}

fn default_speed_percent() -> u16 {
    100
}

/// Human-readable label for a `[floppy] speed` value: "100%".."800%", or
/// "turbo" for `SPEED_TURBO`. Shared by the menu, launcher, and OSD.
pub fn speed_label(percent: u16) -> String {
    if percent == SPEED_TURBO {
        "turbo".to_string()
    } else {
        format!("{percent}%")
    }
}
// 11 AmigaDOS sectors occupy 5984 MFM words. PAL Amiga floppy read timing is
// slightly faster than nominal 250 kbit/s, so a normal 300 RPM revolution is
// about 12668 raw bytes (6334 16-bit MFM words). Keeping generated ADF streams
// at that physical length leaves a realistic index gap for raw trackloaders
// that read fixed-size windows rather than using trackdisk.device.
const STANDARD_ADF_TRACK_WORDS: usize = 6334;
const AMIGADOS_SECTOR_MFM_WORDS: usize = 2 + 2 + (2 + 8 + 2 + 2 + 256) * 2;
const TRACK_GAP_WORDS: usize =
    STANDARD_ADF_TRACK_WORDS - SECTORS_PER_TRACK * AMIGADOS_SECTOR_MFM_WORDS;
const TRACK_TRAILER_WORDS: usize = 2;
const TRACK_GAP_LONGS: usize = (TRACK_GAP_WORDS - TRACK_TRAILER_WORDS) / 2;
const MFM_MASK: u32 = 0x5555_5555;
// Paula's disk write shifter does not emit the final three bits of a write.
const DISK_WRITE_LOST_BITS: usize = 3;
// Paula's reset/default disk-sync word is the AmigaDOS MFM sync mark.
const DEFAULT_DSKSYNC: u16 = 0x4489;
const UAE_EXT1_SIGNATURE: &[u8; 8] = b"UAE--ADF";
const UAE_EXT2_SIGNATURE: &[u8; 8] = b"UAE-1ADF";
const SCP_SIGNATURE: &[u8; 3] = b"SCP";
const GZIP_SIGNATURE: &[u8; 2] = &gzip::SIGNATURE;
const ZIP_SIGNATURE: &[u8; 4] = &[0x50, 0x4b, 0x03, 0x04];

/// The file extensions floppy images conventionally carry, in menu order.
///
/// The loader itself never looks at a name: [`FloppyImage::from_bytes`]
/// decides by signature, which is why an oddly-named image still opens. This
/// list exists only for the places that must offer a *name* filter and cannot
/// sniff -- the desktop file dialogs and the browser page's file picker, both
/// of which hide whatever they do not list. It lives beside the decoder so
/// that adding a format to `decode_floppy_payload` and forgetting the filters
/// is a one-line fix rather than a format that silently cannot be picked.
pub const IMAGE_EXTENSIONS: &[&str] = &["adf", "adz", "dms", "ipf", "scp", "gz", "zip"];

const STANDARD_EXTERNAL_DRIVE_ID: u32 = 0xFFFF_FFFF;
const SCP_TRACK_TABLE_OFFSET: usize = 0x10;
const SCP_EXTENDED_TRACK_TABLE_OFFSET: usize = 0x80;
const SCP_TRACK_TABLE_LEN: usize = SCP_TRACKS * 4;
const SCP_FLAG_INDEX: u8 = 1 << 0;
const SCP_FLAG_RPM_360: u8 = 1 << 2;
const SCP_FLAG_EXTENDED_MODE: u8 = 1 << 6;
const SCP_CAPTURE_BASE_NS: u64 = 25;
const AMIGA_DD_BITCELL_NS: u64 = 2_000;
const SCP_300_RPM_REV_NS: u64 = 200_000_000;
const SCP_360_RPM_REV_NS: u64 = 166_666_667;
const SCP_CHECKSUM_OFFSET: usize = 0x0C;
const SCP_CHECKSUM_START: usize = 0x10;
const MAX_SCP_REVOLUTION_BITS: u32 = 1_000_000;
// Flux-decode PLL (data separator): how strongly the recovered bit-cell window
// tracks the measured per-cell interval, and the range it may drift within.
// Real DD disks spin near 2 us/cell but vary a few percent across a track;
// locking the window to the local flux rate avoids the cumulative drift that a
// fixed-cell rounding accumulates (which corrupts sectors).
const SCP_PLL_GAIN: f64 = 0.15;
const SCP_PLL_MIN_CELL_NS: f64 = 1_500.0;
const SCP_PLL_MAX_CELL_NS: f64 = 2_500.0;

#[cfg(feature = "internal-diagnostics")]
fn disk_speed_div() -> Option<(u32, f64)> {
    use std::sync::OnceLock;
    static V: OnceLock<Option<(u32, f64)>> = OnceLock::new();
    *V.get_or_init(|| {
        let div = crate::envcfg::var("COPPERLINE_DISK_SPEED_DIV")
            .and_then(|s| s.trim().parse::<u32>().ok())?;
        let after = crate::envcfg::var("COPPERLINE_DISK_SPEED_AFTER")
            .and_then(|s| s.trim().parse::<f64>().ok())
            .unwrap_or(0.0);
        Some((div, after))
    })
}

#[cfg(not(feature = "internal-diagnostics"))]
fn disk_speed_div() -> Option<(u32, f64)> {
    None
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct DiskDmaTransfer {
    pub addr: u32,
    pub data: u16,
    /// Direction at the chip-RAM side of the DMA transfer.
    pub memory_write: bool,
    /// Zero-based colour-clock offset within the enclosing `tick` batch.
    pub cck_offset: u32,
    /// The transfer belongs to a turbo burst that consumes no emulated time.
    /// Such transfers cannot share the ordinary one-record-per-clock grid and
    /// are exported in order through the trace's instantaneous-record list.
    pub instantaneous: bool,
}

fn dma_trace_cck_offset(total_data_cck: u32, remaining_data_cck: u32, scale: u32) -> u32 {
    total_data_cck
        .saturating_sub(remaining_data_cck)
        .div_ceil(scale.max(1))
        .saturating_sub(1)
}

#[derive(serde::Serialize, serde::Deserialize)]
pub struct FloppyController {
    /// Debugger: watched word addresses and the last disk-DMA write to
    /// one of them, for watchpoint writer attribution. Transient.
    #[serde(skip)]
    debug_watch_addrs: Vec<u32>,
    #[serde(skip)]
    debug_watched_write: Option<(u32, u16)>,
    /// Actual words transferred by the most recent timed-device tick. This is
    /// enabled only for full bus tracing so ordinary disk DMA stays allocation
    /// free; the Bus drains it immediately after each tick.
    #[serde(skip)]
    trace_dma_enabled: bool,
    #[serde(skip)]
    dma_trace: Vec<DiskDmaTransfer>,
    drives: [FloppyDrive; 4],
    prb: u8,
    side: usize,

    dskpt: u32,
    dsklen: u16,
    dskdat: u16,
    dsksync: u16,
    adkcon: u16,
    last_dskdatr: u16,
    last_dskbytr_byte: u8,
    dskbyte_valid: bool,
    sync_irq_latch: bool,
    index_pulse_cck: u32,
    index_flag_sync_cck: u32,
    index_flag_ready: bool,
    armed_dsklen: Option<u16>,
    dma: Option<DiskDma>,
    direct_write: Option<DiskDirectWrite>,
    dma_addr_mask: u32,
    /// Emulated drive speed: a data-rate percentage from
    /// `SUPPORTED_SPEED_PERCENTS`, or `SPEED_TURBO` (0) for turbo, where DMA
    /// transfers complete almost instantly. Host configuration rather than
    /// machine state: never serialized, carried across save-state loads by
    /// `Bus::adopt_host_resources`.
    #[serde(skip, default = "default_speed_percent")]
    speed_percent: u16,
    /// Turbo: emulated cck left before a freshly armed DMA may burst to
    /// completion (see `TURBO_DMA_GRACE_CCK`). Transient pacing state for a
    /// host speed hack, so not serialized; a state restored mid-grace just
    /// bursts on the next tick.
    #[serde(skip)]
    turbo_grace_cck: u32,
    /// Turbo: the pending DMA already had its burst attempt. A transfer the
    /// burst cannot finish (a sync word missing from the track, a fuzzy
    /// multi-revolution image moving the sync) falls back to normal pacing
    /// instead of rescanning the track on every tick; the next DSKLEN
    /// arming tries again, like FS-UAE's per-DSKLEN turbo scan.
    #[serde(skip)]
    turbo_burst_spent: bool,
    // Head-step pulses since the last take_sound_steps() drain; feeds
    // the synthesized drive sound effects.
    sound_steps: u32,
    // Live Paula read shifter: fed one MFM cell at a time as the selected
    // drive's head rotates, it detects DSKSYNC bit-aligned and frames read-DMA
    // words off the sync bit phase.
    read_shifter: PaulaDiskReadDpllFifo,
    /// Cached `is_idle()` so the per-CPU-access device tick can skip the whole
    /// floppy block (an `is_idle()` recompute, plus the IRQ/sound polling) with
    /// a single bool read while the mechanism is quiescent -- which it is for
    /// almost all of normal running. Set false the moment any register/select
    /// write could activate the drive (conservative; an extra tick at worst),
    /// and recomputed exactly at the top of `tick`. `serde(default)` keeps old
    /// save states loadable: the default `false` just costs one settling tick.
    #[serde(default)]
    idle_cache: bool,
    /// Media changes bridged bays have noticed since the frontend last asked:
    /// `(bay, present, tab)`. `tab` is the inserted disk's write-protect tab,
    /// carried only when the configuration would otherwise allow a write --
    /// the one case where the tab decides anything worth announcing. The log
    /// line has already said it in full; this feeds the same on-screen
    /// message an image insert or eject raises. Host-side and transient, so
    /// never serialized.
    #[cfg(feature = "fluxbridge")]
    #[serde(skip)]
    bridge_media_events: Vec<(usize, bool, Option<bool>)>,
}

impl Default for FloppyController {
    fn default() -> Self {
        let mut drives: [FloppyDrive; 4] = std::array::from_fn(|_| FloppyDrive::default());
        drives[0].assert_no_media_change();
        for drive in drives.iter_mut().skip(1) {
            drive.external_id = 0;
        }
        Self {
            debug_watch_addrs: Vec::new(),
            debug_watched_write: None,
            trace_dma_enabled: false,
            dma_trace: Vec::new(),
            drives,
            prb: 0xFF,
            side: 0,
            dskpt: 0,
            dsklen: 0,
            dskdat: 0,
            dsksync: DEFAULT_DSKSYNC,
            adkcon: 0,
            last_dskdatr: 0,
            last_dskbytr_byte: 0,
            dskbyte_valid: false,
            sync_irq_latch: false,
            index_pulse_cck: 0,
            index_flag_sync_cck: 0,
            index_flag_ready: false,
            armed_dsklen: None,
            dma: None,
            direct_write: None,
            dma_addr_mask: 0x001F_FFFF,
            speed_percent: default_speed_percent(),
            turbo_grace_cck: 0,
            turbo_burst_spent: false,
            sound_steps: 0,
            read_shifter: PaulaDiskReadDpllFifo::new(),
            // Idle at power-on; the first tick confirms it.
            idle_cache: true,
            #[cfg(feature = "fluxbridge")]
            bridge_media_events: Vec::new(),
        }
    }
}

impl FloppyController {
    pub(crate) fn set_dma_trace_enabled(&mut self, enabled: bool) {
        self.trace_dma_enabled = enabled;
        if !enabled {
            self.dma_trace.clear();
        }
    }

    pub(crate) fn take_dma_trace(&mut self) -> Vec<DiskDmaTransfer> {
        std::mem::take(&mut self.dma_trace)
    }

    /// Replace the debugger's watched-address mirror (word-aligned).
    pub fn set_debug_watch_addrs(&mut self, addrs: &[u32]) {
        self.debug_watch_addrs = addrs.to_vec();
    }

    /// Take the last disk-DMA write to a watched address, if any.
    pub fn take_debug_watched_write(&mut self) -> Option<(u32, u16)> {
        self.debug_watched_write.take()
    }

    pub fn from_config(config: &FloppyConfig) -> Result<Self> {
        let mut ctrl = Self { ..Self::default() };
        ctrl.set_speed_percent(config.speed);
        for (idx, drive_cfg) in config.drives.iter().enumerate() {
            if let Some(drive_cfg) = drive_cfg {
                ctrl.drives[idx] = FloppyDrive::load(drive_cfg)
                    .with_context(|| format!("loading floppy.df{}", idx))?;
                debug!(
                    "floppy.df{}: loaded {} write_protected={}",
                    idx,
                    drive_cfg.path.display(),
                    drive_cfg.write_protected
                );
            }
        }
        ctrl.write_prb(ctrl.prb);
        Ok(ctrl)
    }

    pub fn set_connected_drives(&mut self, connected: [bool; 4]) {
        for (idx, drive) in self.drives.iter_mut().enumerate() {
            drive.external_id = if connected[idx] {
                STANDARD_EXTERNAL_DRIVE_ID
            } else {
                0
            };
            drive.reset_external_signal();
        }
    }

    pub fn set_dma_addr_mask(&mut self, mask: u32) {
        self.dma_addr_mask = mask | 1;
        self.dskpt &= self.dma_ptr_mask();
    }

    /// Set the emulated drive speed: a percentage from
    /// `SUPPORTED_SPEED_PERCENTS` or `SPEED_TURBO` (0). Unsupported values
    /// fall back to real speed. Takes effect immediately; drive mechanics
    /// (motor spin-up, stepping, settle) are never accelerated.
    pub fn set_speed_percent(&mut self, percent: u16) {
        self.speed_percent =
            if percent == SPEED_TURBO || SUPPORTED_SPEED_PERCENTS.contains(&percent) {
                percent
            } else {
                default_speed_percent()
            };
        // Entering turbo starts a fresh grace window and burst attempt, so a
        // live toggle with a DMA already in flight defers its burst exactly
        // like a freshly armed transfer would, instead of completing on the
        // very next tick (or staying blocked by a spent attempt from an
        // earlier turbo phase).
        if self.turbo() {
            self.turbo_grace_cck = TURBO_DMA_GRACE_CCK;
            self.turbo_burst_spent = false;
        } else {
            self.turbo_grace_cck = 0;
        }
    }

    pub fn speed_percent(&self) -> u16 {
        self.speed_percent
    }

    /// Whole multiple of real speed the data path runs at. Turbo paces the
    /// platter at real speed between bursts, so it maps to 1 here.
    fn speed_multiplier(&self) -> u32 {
        match self.speed_percent {
            0..=100 => 1,
            p => u32::from(p / 100),
        }
    }

    fn turbo(&self) -> bool {
        self.speed_percent == SPEED_TURBO
    }

    /// The data-rate multiple `drive_idx` actually runs at. `[floppy] speed`
    /// shapes how fast a track is served from an image; a physical drive's
    /// data rate is the disk's own, and accelerating the emulated side of it
    /// only makes the guest outrun the cells the drive is delivering. A
    /// bridged bay therefore takes no multiplier here; its own
    /// `bridge_speed` instead compresses how fast the captured revolution
    /// is served.
    fn drive_speed_multiplier(&self, drive_idx: usize) -> u32 {
        if self.drives[drive_idx].is_bridged() {
            1
        } else {
            self.speed_multiplier()
        }
    }

    /// Whether turbo applies to `drive_idx`: never to a bridged bay, whose
    /// platter cannot be spun forward in zero time.
    fn drive_turbo(&self, drive_idx: usize) -> bool {
        self.turbo() && !self.drives[drive_idx].is_bridged()
    }

    pub fn cia_a_status_bits(&self) -> u8 {
        let Some(idx) = self.selected_drive() else {
            return CIAA_DSKCHANGE | CIAA_DSKPROT | CIAA_DSKTRACK0 | CIAA_DSKRDY;
        };
        let drive = &self.drives[idx];

        let mut bits = 0u8;
        if !drive.disk_change_sense {
            bits |= CIAA_DSKCHANGE;
        }
        if !drive.write_protected_sense {
            bits |= CIAA_DSKPROT;
        }
        // The TRACK0 optical sensor follows the head carriage's physical
        // position, which moves on each step edge. It must NOT be gated by the
        // data-readability settle delay: a trackloader recalibrating by
        // stepping outward and polling /TRK0 between fast step pulses would
        // otherwise never see track 0 (the settle lags the whole multi-step
        // seek) and hang.
        if drive.cylinder != 0 {
            bits |= CIAA_DSKTRACK0;
        }
        // The internal DF0 mechanism has no drive-ID circuit; with its motor
        // off, the /RDY line it presents mirrors the /TRK0 sensor instead
        // (the behaviour WinUAE derived from A500/A2000 hardware, where the
        // external units answer with their ID shift register). With the
        // motor on, /RDY is the spun-up platter as on every drive.
        let rdy_asserted = if idx == 0 && !drive.motor_on {
            drive.cylinder == 0
        } else {
            drive.rdy_line_asserted()
        };
        if !rdy_asserted {
            bits |= CIAA_DSKRDY;
        }
        bits
    }

    /// Whether a DSKLEN write of `val` right now would be the one that
    /// actually starts a transfer.
    ///
    /// Paula requires the value written twice in succession as a safety
    /// interlock: the first write only latches it. Asked before the
    /// write is dispatched, so it reads the latch the previous write
    /// left.
    pub fn dsklen_write_starts_dma(&self, val: u16) -> bool {
        val & DSKLEN_DMAEN != 0 && self.dma.is_none() && self.armed_dsklen == Some(val)
    }

    /// Whether a disk-DMA arming right now would find a drive able to
    /// serve it: a selected drive, its motor spinning, and media in it.
    /// Returns the `regcheck::DISK_*` code for what is missing, so the
    /// wording of the report lives in one place rather than being
    /// recovered from a string here.
    pub fn dma_arming_obstacle(&self) -> Option<u16> {
        let Some(idx) = self.selected_drive() else {
            return Some(crate::regcheck::DISK_NO_DRIVE);
        };
        if !self.drives[idx].motor_on {
            return Some(crate::regcheck::DISK_MOTOR_OFF);
        }
        if self.drives[idx].cached.is_empty() {
            return Some(crate::regcheck::DISK_EMPTY);
        }
        None
    }

    pub fn activity_led_on(&self) -> bool {
        self.selected_drive()
            .is_some_and(|idx| self.drives[idx].motor_on)
    }

    pub fn selected_track(&self) -> Option<u8> {
        self.selected_drive()
            .map(|idx| self.track_for_drive(idx) as u8)
    }

    /// Whether a drive is wired up. The connectivity marker shares the
    /// external-ID field: DF1-DF3 return that ID over the daisy-chain
    /// protocol, while DF0 only uses its non-zero value to represent that an
    /// internal mechanism is fitted.
    pub fn drive_connected(&self, drive_idx: usize) -> bool {
        self.drives
            .get(drive_idx)
            .is_some_and(|drive| drive.external_id != 0)
    }

    pub fn disk_inserted(&self, drive_idx: usize) -> bool {
        self.drives
            .get(drive_idx)
            .is_some_and(|drive| drive.image.is_some())
    }

    /// File name of the inserted image, for UI labels. Read from the image
    /// itself rather than any host-side playlist, so CLI and config-embedded
    /// inserts are covered too.
    pub fn inserted_disk_name(&self, drive_idx: usize) -> Option<String> {
        let image = self.drives.get(drive_idx)?.image.as_ref()?;
        Some(
            image
                .path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| image.path.display().to_string()),
        )
    }

    /// Whether the image in DFn presents a closed write-protect tab.
    pub fn disk_image_write_protected(&self, drive_idx: usize) -> Option<bool> {
        Some(self.drives.get(drive_idx)?.image.as_ref()?.write_protected)
    }

    /// Encode the current in-memory image as a standalone ADF. Standard ADFs
    /// stay byte-for-byte standard; track images become the same UAE extended
    /// ADF variant they were loaded as. Container formats such as ADZ and DMS
    /// deliberately export their decoded disk rather than recompressing it.
    pub fn export_disk_image(&self, drive_idx: usize) -> Result<Vec<u8>> {
        let image = self
            .drives
            .get(drive_idx)
            .with_context(|| format!("invalid floppy drive df{drive_idx}"))?
            .image
            .as_ref()
            .with_context(|| format!("floppy.df{drive_idx} is empty"))?;
        match &image.data {
            FloppyImageData::StandardAdf(data) => Ok(data.clone()),
            FloppyImageData::Tracks(tracks) if image.legacy_extended_adf => {
                encode_uae_legacy_extended_adf(tracks)
            }
            FloppyImageData::Tracks(tracks) => encode_uae_extended_adf(tracks),
        }
    }

    /// Capture disk media for netplay without losing raw-track timing or
    /// legacy sync metadata. The result is accepted by the memory-backed
    /// loader, contains no filesystem paths, and is bounded before copying.
    pub fn export_netplay_disk_image(&self, drive_idx: usize, limit: usize) -> Result<Vec<u8>> {
        let image = self
            .drives
            .get(drive_idx)
            .and_then(|drive| drive.image.as_ref())
            .with_context(|| format!("floppy.df{drive_idx} is empty or invalid"))?;
        transfer::encode(image, limit)
    }

    /// Adopt disk contents for rollback and remove host-specific path metadata.
    /// Memory backing prevents guest writes from escaping a netplay session.
    pub fn prepare_netplay_images(&mut self) {
        for (index, drive) in self.drives.iter_mut().enumerate() {
            if let Some(image) = drive.image.as_mut() {
                image.backing = FloppyImageBacking::Memory;
                image.path = format!("netplay-df{index}").into();
            }
        }
    }

    /// A host without a writable filesystem adopts every restored image into
    /// memory. This is intentionally called after browser save-state loads:
    /// desktop states may name writable host files which do not exist there.
    pub fn make_disk_images_memory_backed(&mut self) {
        for image in self
            .drives
            .iter_mut()
            .filter_map(|drive| drive.image.as_mut())
        {
            image.backing = FloppyImageBacking::Memory;
        }
    }

    pub fn insert_disk_image(
        &mut self,
        drive_idx: usize,
        path: PathBuf,
        write_protected: bool,
    ) -> Result<()> {
        ensure!(
            drive_idx < self.drives.len(),
            "invalid floppy drive df{}",
            drive_idx
        );
        ensure!(
            self.drive_connected(drive_idx),
            "floppy.df{drive_idx} is not connected"
        );
        // A bay is either a real drive or an image, never both: the bridge
        // keeps supplying the track under the head, so an image mounted on top
        // would be reported as inserted and then never read. The status bar
        // greys its own buttons, but scheduled inserts, drag-and-drop and the
        // control protocol all arrive here instead, so the invariant belongs
        // where every route passes.
        #[cfg(feature = "fluxbridge")]
        ensure!(
            !self.drives[drive_idx].is_bridged(),
            "floppy.df{drive_idx} is a physical drive; take the drive off the bay before \
             using a disk image in it"
        );
        let config = FloppyDriveConfig {
            path,
            write_protected,
        };
        let image = FloppyImage::load(&config)
            .with_context(|| format!("loading floppy.df{} image", drive_idx))?;
        self.idle_cache = false;
        self.drives[drive_idx].insert_image(image);
        if self.selected_drive() == Some(drive_idx) {
            self.ensure_track(drive_idx, self.track_for_drive(drive_idx));
        }
        Ok(())
    }

    /// Insert a disk from already-loaded image bytes while retaining ordinary
    /// host-file write-through semantics. `label` is both its UI name and the
    /// path updated by a writable image.
    pub fn insert_disk_image_bytes(
        &mut self,
        drive_idx: usize,
        bytes: Vec<u8>,
        label: PathBuf,
        write_protected: bool,
    ) -> Result<()> {
        ensure!(
            drive_idx < self.drives.len(),
            "invalid floppy drive df{}",
            drive_idx
        );
        ensure!(
            self.drive_connected(drive_idx),
            "floppy.df{drive_idx} is not connected"
        );
        // A bay is either a real drive or an image, never both: the bridge
        // keeps supplying the track under the head, so an image mounted on top
        // would be reported as inserted and then never read. The status bar
        // greys its own buttons, but scheduled inserts, drag-and-drop and the
        // control protocol all arrive here instead, so the invariant belongs
        // where every route passes.
        #[cfg(feature = "fluxbridge")]
        ensure!(
            !self.drives[drive_idx].is_bridged(),
            "floppy.df{drive_idx} is a physical drive; take the drive off the bay before \
             using a disk image in it"
        );
        let image = FloppyImage::from_bytes(bytes, label, write_protected)
            .with_context(|| format!("loading floppy.df{} image", drive_idx))?;
        self.idle_cache = false;
        self.drives[drive_idx].insert_image(image);
        if self.selected_drive() == Some(drive_idx) {
            self.ensure_track(drive_idx, self.track_for_drive(drive_idx));
        }
        Ok(())
    }

    /// Insert a disk whose writable backing lives entirely in this controller.
    /// Guest writes update the serialized image data and never touch `label`;
    /// [`Self::export_disk_image`] snapshots the current bytes for its host.
    /// Formats without a writable representation (compressed containers,
    /// DMS, IPF and SCP) reject a writable request instead of silently
    /// presenting a write-protected disk.
    pub fn insert_memory_disk_image_bytes(
        &mut self,
        drive_idx: usize,
        bytes: Vec<u8>,
        label: PathBuf,
        write_protected: bool,
    ) -> Result<()> {
        self.insert_memory_disk_image_bytes_with_limit(
            drive_idx,
            bytes,
            label,
            write_protected,
            GZIP_IMAGE_LIMIT,
        )
    }

    /// Insert memory-backed media with a byte limit on both the supplied file
    /// and its expanded gzip/zip payload. A rejected image leaves the drive
    /// unchanged, including when decompression reaches the limit.
    pub fn insert_memory_disk_image_bytes_with_limit(
        &mut self,
        drive_idx: usize,
        bytes: Vec<u8>,
        label: PathBuf,
        write_protected: bool,
        limit: usize,
    ) -> Result<()> {
        ensure!(
            drive_idx < self.drives.len(),
            "invalid floppy drive df{}",
            drive_idx
        );
        ensure!(
            self.drive_connected(drive_idx),
            "floppy.df{drive_idx} is not connected"
        );
        #[cfg(feature = "fluxbridge")]
        ensure!(
            !self.drives[drive_idx].is_bridged(),
            "floppy.df{drive_idx} is a physical drive; take the drive off the bay before \
             using a disk image in it"
        );
        ensure!(bytes.len() <= limit, "floppy image exceeds {limit} bytes");
        let image = FloppyImage::from_memory_bytes(bytes, label, write_protected, limit)
            .with_context(|| format!("loading floppy.df{} image", drive_idx))?;
        ensure!(
            write_protected || !image.write_protected,
            "floppy image {} uses a read-only format; use an uncompressed ADF or UAE extended ADF for writable media",
            image.path.display()
        );
        self.idle_cache = false;
        self.drives[drive_idx].insert_image(image);
        if self.selected_drive() == Some(drive_idx) {
            self.ensure_track(drive_idx, self.track_for_drive(drive_idx));
        }
        Ok(())
    }

    pub fn eject_disk_image(&mut self, drive_idx: usize) -> Result<()> {
        ensure!(
            drive_idx < self.drives.len(),
            "invalid floppy drive df{}",
            drive_idx
        );
        // A bay is either a real drive or an image, never both: the bridge
        // keeps supplying the track under the head, so an image mounted on top
        // would be reported as inserted and then never read. The status bar
        // greys its own buttons, but scheduled inserts, drag-and-drop and the
        // control protocol all arrive here instead, so the invariant belongs
        // where every route passes.
        #[cfg(feature = "fluxbridge")]
        ensure!(
            !self.drives[drive_idx].is_bridged(),
            "floppy.df{drive_idx} is a physical drive; take the drive off the bay before \
             using a disk image in it"
        );
        self.idle_cache = false;
        self.drives[drive_idx].eject_image();
        Ok(())
    }

    /// Put a real drive on `drive_idx`, replacing any mounted image. The
    /// bridge supplies the track under the head from then on. `speed_percent`
    /// is the serving speed from `[floppy.dfN] bridge_speed`, already
    /// validated against `SUPPORTED_BRIDGE_SPEED_PERCENTS`.
    #[cfg(feature = "fluxbridge")]
    pub fn attach_bridge(
        &mut self,
        drive_idx: usize,
        bridge: crate::fluxbridge::Bridge,
        write_protected: bool,
        speed_percent: u16,
    ) -> Result<()> {
        ensure!(
            drive_idx < self.drives.len(),
            "invalid floppy drive df{drive_idx}"
        );
        self.idle_cache = false;
        let drive = &mut self.drives[drive_idx];
        drive.eject_image();
        // Either protection is enough to keep the disk read-only.
        drive.bridge_write_protected = write_protected;
        drive.bridge_tab_write_protected = bridge.write_protected();
        drive.bridge_media = bridge.disk_in_drive();
        drive.write_protected_target =
            write_protected || (drive.bridge_media && drive.bridge_tab_write_protected);
        drive.bridge_speed_percent = speed_percent.max(100);
        drive.bridge = Some(bridge);
        // Announce the swap so the guest re-reads rather than trusting
        // whatever it last saw in this drive.
        drive.set_disk_change(true);
        Ok(())
    }

    /// Whether any bay is a physical drive. Such a machine cannot be run
    /// faster than real time: the platter turns at its own speed and nothing
    /// the emulator does will hurry it.
    #[cfg(feature = "fluxbridge")]
    pub fn has_bridged_drive(&self) -> bool {
        self.drives.iter().any(|d| d.bridge.is_some())
    }

    /// Why this controller cannot be replayed speculatively. A physical
    /// mechanism cannot rewind, and a file-backed writable image flushes
    /// completed guest writes to its host file. Read-only and memory-backed
    /// image media are fully serialized.
    pub fn runahead_block_reason(&self) -> Option<&'static str> {
        #[cfg(feature = "fluxbridge")]
        if self.drives.iter().any(FloppyDrive::is_bridged) {
            return Some("physical floppy drive");
        }
        if self.drives.iter().any(|drive| {
            drive.image.as_ref().is_some_and(|image| {
                !image.write_protected && image.backing == FloppyImageBacking::File
            })
        }) {
            return Some("writable floppy image");
        }
        None
    }

    /// Whether any fitted bay serves from an image. Drive speed only shapes
    /// how fast a track is served from one: a real drive's data rate is the
    /// disk's own, so with every fitted bay physical there is nothing for the
    /// setting to act on.
    pub fn has_image_drive(&self) -> bool {
        (0..self.drives.len()).any(|idx| {
            #[cfg(feature = "fluxbridge")]
            let bridged = self.is_bridged(idx);
            #[cfg(not(feature = "fluxbridge"))]
            let bridged = false;
            self.drive_connected(idx) && !bridged
        })
    }

    /// Let go of every real drive, closing the device and handing the port
    /// back to the host.
    ///
    /// This is what powering the machine off has to do. A `Bridge` holds the
    /// interface open for as long as it exists, and the library keeps its
    /// worker running behind it -- so a machine that merely stopped stepping
    /// would leave the real drive clicking away as though the Amiga were still
    /// on, and nothing else could open the device. Dropping the bridge closes
    /// it, exactly as cutting the power to a real drive would.
    ///
    /// The bays revert to empty image-backed drives, so a machine that carries
    /// on running simply has nothing in them.
    #[cfg(feature = "fluxbridge")]
    pub fn release_bridges(&mut self) {
        for (idx, drive) in self.drives.iter_mut().enumerate() {
            if drive.bridge.take().is_none() {
                continue;
            }
            info!("floppy.df{idx} physical drive released (interface handed back to the host)");
            drive.bridge_media = false;
            drive.cached_track = None;
            drive.cached = CachedTrack::default();
            drive.bridge_filler_track = None;
            drive.bridge_partial_track = None;
            // The drive is gone; a write still in flight will never report.
            drive.bridge_writing.clear();
            drive.bridge_tracks.clear();
            drive.eject_image();
            self.idle_cache = false;
        }
    }

    /// Whether this drive is backed by a real drive.
    #[cfg(feature = "fluxbridge")]
    pub fn is_bridged(&self, drive_idx: usize) -> bool {
        self.drives
            .get(drive_idx)
            .is_some_and(|d| d.bridge.is_some())
    }

    /// Point a bridged drive's real head where the emulated one is.
    ///
    /// A real drive's head follows the stepper, so a bridged one does too. It
    /// also buys back most of the wait for a track: the driver starts capturing
    /// the moment the head arrives, so by the time the guest gets round to
    /// reading, the revolution is often already in hand. The library coalesces
    /// queued moves, so a trackloader sweeping across the disk does not pile up
    /// a seek per cylinder.
    #[cfg(feature = "fluxbridge")]
    fn sync_bridge_head(&mut self, idx: usize) {
        let side = self.side != 0;
        let Some(drive) = self.drives.get_mut(idx) else {
            return;
        };
        let cylinder = drive.cylinder;
        if let Some(bridge) = drive.bridge.as_mut() {
            if bridge_diag() {
                info!(
                    "fluxbridge.df{idx} head to cylinder {cylinder} side {} (drive at {})",
                    u8::from(side),
                    bridge.current_cylinder(),
                );
            }
            bridge.seek(cylinder, side);
        }
    }

    /// Notice a disk swapped by hand in a bridged drive. Unlike an image, the
    /// medium can change without the emulator being told, so the frontend
    /// polls this (once a frame is ample) to raise the change line.
    #[cfg(feature = "fluxbridge")]
    pub fn poll_bridge_media(&mut self) {
        for (idx, drive) in self.drives.iter_mut().enumerate() {
            let Some(bridge) = drive.bridge.as_mut() else {
                continue;
            };
            // An interface pulled out mid-session stops answering rather than
            // reporting anything useful, and the guest just sees a drive that
            // has gone quiet. Say it once, so the reason is in the log.
            let working = bridge.is_working();
            if !working && !drive.bridge_reported_failed {
                warn!(
                    "floppy.df{idx} the physical drive's interface has stopped responding \
                     (unplugged?); this drive will not read or write until it is \
                     reconnected and the machine restarted"
                );
            }
            drive.bridge_reported_failed = !working;
            // What became of the writes handed over earlier. A track is held
            // unreadable from the moment its write is accepted until one of
            // these arrives, so this is also what releases it -- and it is
            // released whether the write worked or not, because either way
            // the platter now holds something the guest has not seen.
            for outcome in bridge.poll_write_outcomes() {
                let track = usize::from(outcome.cylinder) * SIDES + usize::from(outcome.side);
                drive.bridge_writing.retain(|pending| *pending != track);
                if let Some(known) = drive.bridge_tracks.get_mut(track) {
                    *known = BridgeTrack::Unknown;
                }
                if drive.cached_track == Some(track) {
                    drive.cached_track = None;
                }
                match &outcome.error {
                    // A real write is only known to have worked once the disk
                    // has turned, so a failure lands here rather than at the
                    // instruction that caused it. Saying so is the difference
                    // between a disk the guest believes it wrote and one it
                    // knows it did not.
                    Some(error) => warn!(
                        "floppy.df{idx} the write of track {track} (cyl {} side {}) did not \
                         reach the disk: {error}",
                        outcome.cylinder,
                        u8::from(outcome.side)
                    ),
                    None if bridge_diag() => info!(
                        "fluxbridge.df{idx} track {track} (cyl {} side {}) is on the platter",
                        outcome.cylinder,
                        u8::from(outcome.side)
                    ),
                    None => {}
                }
            }
            let changed = bridge.take_disk_changed();
            let had_media = drive.bridge_media;
            drive.bridge_media = bridge.disk_in_drive();
            // Sample the drive before the borrow is needed elsewhere. The
            // driver keeps the tab's last reading and hands it back whatever
            // the motor is doing, so this is good with the platter stopped --
            // which is just as well, because a drive the guest is not actively
            // reading is stopped nearly all the time.
            let sensed_tab = bridge.write_protected();
            // A disk going in or coming out of a real drive is the one media
            // change nothing in the emulator asked for, so it is worth saying
            // as plainly as an image being inserted. A drive the
            // configuration protects cannot be written to whatever the tab
            // says, so the tab is only worth reporting on a drive that could
            // otherwise take a write.
            if drive.bridge_media != had_media {
                if drive.bridge_media {
                    if drive.bridge_write_protected {
                        info!("floppy.df{idx} disk inserted (physical drive)");
                    } else {
                        info!(
                            "floppy.df{idx} disk inserted (physical drive), {}",
                            if sensed_tab {
                                "write-protected by the disk's tab"
                            } else {
                                "writable"
                            }
                        );
                    }
                } else {
                    info!("floppy.df{idx} disk ejected (physical drive)");
                }
                // Bounded in case nothing ever drains it (a headless run):
                // these are hand-speed events, and the frontend takes them
                // every frame, so the cap is never met in a windowed session.
                if self.bridge_media_events.len() < 64 {
                    let tab =
                        (drive.bridge_media && !drive.bridge_write_protected).then_some(sensed_tab);
                    self.bridge_media_events
                        .push((idx, drive.bridge_media, tab));
                }
            }
            if changed {
                drive.cached_track = None;
                drive.cached = CachedTrack::default();
                drive.bridge_filler_track = None;
                drive.bridge_partial_track = None;
                drive.bridge_tracks.clear();
                drive.set_disk_change(true);
                self.idle_cache = false;
            }
            drive.bridge_tab_write_protected = sensed_tab;
            // With no disk in the drive there is no tab to have an opinion,
            // and announcing one reads as though something were in there.
            // The configured protection still stands on its own.
            let write_protected = drive.bridge_write_protected
                || (drive.bridge_media && drive.bridge_tab_write_protected);
            if write_protected != drive.write_protected_target {
                // Which of the two protections is in force decides whether
                // opening the tab will help, so name it rather than just
                // reporting the outcome. An insertion has already said this,
                // and an empty drive has no disk to say it about.
                if drive.bridge_media && drive.bridge_media == had_media {
                    if write_protected {
                        let reason = if drive.bridge_write_protected {
                            "the configuration"
                        } else {
                            "the disk's tab"
                        };
                        info!(
                            "floppy.df{idx} disk is write-protected by {reason} (physical drive)"
                        );
                    } else {
                        info!("floppy.df{idx} disk is writable (physical drive)");
                    }
                }
                drive.set_write_protected(write_protected);
                self.idle_cache = false;
            }
        }
    }

    pub fn reset_external_drives(&mut self) {
        self.idle_cache = false;
        for drive in self.drives.iter_mut().skip(1) {
            drive.reset_external_signal();
        }
        self.index_pulse_cck = 0;
        self.index_flag_sync_cck = 0;
        self.index_flag_ready = false;
    }

    pub fn write_prb(&mut self, val: u8) {
        // Drive select / motor / step may wake the mechanism; force the device
        // tick to re-evaluate (recomputed exactly in `tick`).
        self.idle_cache = false;
        let prev = self.prb;
        self.prb = val;
        // DSKSIDE is active-low on Amiga drives: 0 selects the upper
        // head, which maps to odd ADF tracks. Lower/even is selected
        // when the bit is high.
        let side_changed = self.side != if val & CIAB_DSKSIDE == 0 { 1 } else { 0 };
        self.side = if val & CIAB_DSKSIDE == 0 { 1 } else { 0 };

        for idx in 0..self.drives.len() {
            if !self.drive_connected(idx) {
                continue;
            }
            let select_mask = CIAB_DSKSEL_MASKS[idx];
            let was_selected = prev & select_mask == 0;
            let selected = val & select_mask == 0;
            let select_activated = !was_selected && selected;
            let select_deactivated = was_selected && !selected;

            // Every Amiga drive, the internal DF0 included, feeds /MTR into
            // a latch clocked by its own /SEL falling edge: the motor state
            // changes only at the instant the drive becomes selected, and
            // the level of the MTR line while it stays selected is ignored.
            // That is why trackdisk.device and every trackloader deselect,
            // set MTR, then reselect -- and why a loader may restore an
            // old PRB shadow with MTR high right after that sequence
            // without stopping the motor it just started (Spooky Town's
            // loader does exactly this under Kickstart 1.3). A write that
            // drops SEL and MTR together starts the motor -- the latch sees
            // MTR low on one side of the edge -- while stopping it needs MTR
            // high on both sides, the rule WinUAE and vAmiga derived from
            // hardware. External units additionally run the drive-ID shift
            // register off the same edges; DF0 has no ID circuit.
            if select_activated {
                let motor_on = (prev & CIAB_DSKMOTOR == 0) || (val & CIAB_DSKMOTOR == 0);
                if idx == 0 {
                    self.drives[idx].set_motor(motor_on);
                } else {
                    self.drives[idx].latch_mtrxd(motor_on);
                }
            } else if idx != 0 && select_deactivated {
                self.drives[idx].advance_external_id();
            }

            let step_falling_edge = (prev & CIAB_DSKSTEP != 0) && (val & CIAB_DSKSTEP == 0);
            if selected && step_falling_edge {
                // The drive latches the direction line present at the STEP
                // edge, which is the value being written (val), not the prior
                // PRB state. Some trackloaders set DSKDIREC in the same write
                // that drives the step pulse, so sampling `prev` would step
                // the wrong way on the first move after a direction change.
                let inward = val & CIAB_DSKDIREC == 0;
                let stepper_fired = self.drives[idx].step(inward);
                self.handle_active_dma_track_change(idx);
                // Only pulses that reach the stepper are audible.
                // Trackdisk's no-disk change-line polling makes the
                // classic empty-drive click, but the drive gates
                // outward pulses with the /TRK0 sensor at cylinder 0
                // (a silent poll, which NoClick patches rely on), and
                // pulses faster than the mechanism move nothing.
                // A real drive on a bridge makes its own noise across the
                // room; synthesizing a second click on top would be a drive
                // heard twice. Per drive, so an image in the next bay still
                // clicks as it should.
                if stepper_fired && !self.drives[idx].is_bridged() {
                    self.sound_steps = self.sound_steps.saturating_add(1);
                }
                #[cfg(feature = "fluxbridge")]
                if stepper_fired {
                    self.sync_bridge_head(idx);
                }
            }

            if was_selected != selected {
                debug!("floppy.df{} selected={}", idx, selected);
            }
        }
        if let Some(idx) = self.selected_drive() {
            #[cfg(feature = "fluxbridge")]
            if side_changed {
                self.sync_bridge_head(idx);
            }
            self.ensure_track(idx, self.track_for_drive(idx));
        }
        #[cfg(not(feature = "fluxbridge"))]
        let _ = side_changed;
    }

    pub fn set_dskpt_high(&mut self, val: u16) {
        self.dskpt =
            ((self.dskpt & 0x0000_FFFE) | (((val as u32) & 0x001F) << 16)) & self.dma_ptr_mask();
    }

    pub fn set_dskpt_low(&mut self, val: u16) {
        self.dskpt = ((self.dskpt & 0x001F_0000) | ((val as u32) & 0xFFFE)) & self.dma_ptr_mask();
    }

    pub fn write_dskdat(&mut self, val: u16) {
        self.idle_cache = false;
        self.dskdat = val;
        if self.dma.is_some() || self.dsklen & DSKLEN_WRITE == 0 {
            return;
        }
        let remaining = self.dsklen & DSKLEN_MASK;
        if remaining == 0 {
            return;
        }
        let Some((idx, track)) = self.selected_ready_track() else {
            return;
        };
        self.ensure_track(idx, track);
        let write_start_word = self.drives[idx].rotation_bit / 16;
        let write_start_bit = (self.drives[idx].rotation_bit % 16) as u8;

        let replace_direct = self
            .direct_write
            .as_ref()
            .is_some_and(|direct| direct.drive != idx || direct.track != track);
        if replace_direct {
            if let Some(direct) = self.direct_write.take() {
                self.finish_direct_write(direct);
            }
        }
        if self.direct_write.is_none() {
            self.direct_write = Some(DiskDirectWrite {
                drive: idx,
                track,
                write_words: Vec::new(),
                write_start_word,
                write_start_bit,
            });
        }
        if let Some(direct) = self.direct_write.as_mut() {
            direct.write_words.push(val);
        }

        let next_remaining = remaining.saturating_sub(1);
        self.dsklen = (self.dsklen & !DSKLEN_MASK) | next_remaining;
        let is_selected = self.selected_drive() == Some(idx);
        let mut index_pulse = false;
        for _ in 0..16 {
            if self.drives[idx].advance_head_bit() {
                index_pulse = true;
            }
        }
        if index_pulse && is_selected {
            self.start_index_pulse();
        }
        if next_remaining == 0 {
            if let Some(direct) = self.direct_write.take() {
                self.finish_direct_write(direct);
            }
        }
    }

    /// Mirror Paula's ADKCON into the disk controller so the free-running
    /// sync comparator can see the current WORDSYNC/MSBSYNC mode. Paula's
    /// disk-sync detector runs on the live MFM read stream whenever a drive
    /// is selected and spinning, not only during disk DMA.
    pub fn set_adkcon(&mut self, val: u16) {
        self.adkcon = val;
    }

    pub fn write_dsksync(&mut self, val: u16) -> bool {
        self.dsksync = val;
        self.read_shifter.set_sync(val);
        if self.current_disk_word_matches_sync() {
            self.record_sync_match();
            true
        } else {
            false
        }
    }

    pub fn write_dsklen(&mut self, val: u16, adkcon: u16) -> bool {
        self.idle_cache = false;
        self.dsklen = val;

        if val & DSKLEN_DMAEN == 0 {
            if let Some(dma) = self.dma.take() {
                if crate::envcfg::flag("COPPERLINE_DIAG_DISK") {
                    log::info!(
                        "disk-dma teardown df{} track={} write={} remaining={} wait_sync={}",
                        dma.drive,
                        dma.track,
                        dma.write,
                        dma.remaining,
                        dma.wait_sync,
                    );
                }
                if dma.write && !dma.write_words.is_empty() {
                    self.finish_write_dma(dma);
                }
            }
            if let Some(direct) = self.direct_write.take() {
                self.finish_direct_write(direct);
            }
            self.armed_dsklen = None;
            return false;
        }

        if val & DSKLEN_WRITE == 0 {
            if let Some(direct) = self.direct_write.take() {
                self.finish_direct_write(direct);
            }
            if self.dma.as_ref().is_some_and(|dma| !dma.write) {
                let remaining = (val & DSKLEN_MASK) as u32;
                self.armed_dsklen = None;
                if remaining == 0 && self.dma.as_ref().is_some_and(|dma| !dma.wait_sync) {
                    if let Some(dma) = self.dma.take() {
                        self.finish_dma(dma);
                    }
                    return true;
                }
                if let Some(dma) = self.dma.as_mut() {
                    dma.remaining = remaining;
                }
                return false;
            }
        } else if self.dma.as_ref().is_some_and(|dma| dma.write) {
            let remaining = (val & DSKLEN_MASK) as u32;
            self.armed_dsklen = None;
            if remaining == 0 {
                if let Some(dma) = self.dma.take() {
                    self.finish_dma(dma);
                }
                return true;
            }
            if let Some(dma) = self.dma.as_mut() {
                dma.remaining = remaining;
            }
            return false;
        }

        if self.armed_dsklen != Some(val) {
            self.armed_dsklen = Some(val);
            return false;
        }
        self.armed_dsklen = None;
        self.start_dma(val, adkcon)
    }

    pub fn read_dskdatr(&mut self) -> u16 {
        if let Some((idx, track)) = self.selected_ready_track() {
            self.ensure_track(idx, track);
            if let Some(word) = self.peek_head_word(idx) {
                self.last_dskdatr = word;
            }
        }
        self.last_dskdatr
    }

    /// DSKBYTR. The byte and DSKBYT come from Paula's read shifter on its
    /// own framing: the bit counter that a DSKSYNC match resets under
    /// WORDSYNC. After a sync, the bytes a polling reader collects therefore
    /// start exactly where the sync word ended, whatever bit phase that sync
    /// has on the track -- an IPF or flux track carries its syncs at any
    /// phase, and only a synthesised AmigaDOS track keeps them on the word
    /// grid. (Rob Northen's Copylock reads its key sector this way: it waits
    /// for WORDEQUAL, then reads the sector byte by byte through DSKBYTR
    /// while counting poll iterations, and rejects the read unless the first
    /// two bytes are the MFM of the sector's first data byte.) WORDEQUAL is
    /// the comparator's live level: true for the one cell in which the shift
    /// register equals DSKSYNC and gone once the next cell shifts in, so a
    /// polling loop can miss a match and wait a revolution for the next --
    /// as it does on the hardware. Only DSKBYT is reset by the read.
    pub fn read_dskbytr(&mut self, dmacon: u16) -> u16 {
        let mut status = 0u16;
        if self.dma_enabled(dmacon) {
            status |= DMAON;
        }
        if self.dsklen & DSKLEN_WRITE != 0 {
            status |= DISKWRITE;
        }
        // Write mode with the DMA disarmed (DSKLEN WRITE without DMAEN) holds
        // the byte register: nothing loads until the write bit clears.
        let load_allowed = self.dsklen & (DSKLEN_DMAEN | DSKLEN_WRITE) != DSKLEN_WRITE;
        let active_write_dma = self.dma.as_ref().is_some_and(|dma| dma.write);
        if let Some(byte) = self.read_shifter.take_dskbyt() {
            if load_allowed {
                // During a write transfer the shifter is Paula's output path
                // and the byte register reads as zero.
                self.last_dskbytr_byte = if active_write_dma { 0 } else { byte };
                self.dskbyte_valid = true;
            }
        }
        if self.read_shifter.sync_matched() {
            status |= WORDEQUAL;
        }
        if self.dskbyte_valid {
            status |= DSKBYT;
            self.dskbyte_valid = false;
        }
        status | self.last_dskbytr_byte as u16
    }

    /// True when advancing time changes nothing observable: no transfer is
    /// scheduled, no index timing is in flight, no drive is selected, and
    /// every drive is fully spun down and settled. In that state `tick`
    /// only accumulates each drive's diagnostic `elapsed_cck` (read solely
    /// behind `COPPERLINE_DIAG_DISK` at DMA start), so it can be skipped
    /// entirely. Spans most of the time an Amiga spends not using the disk.
    fn is_idle(&self) -> bool {
        self.dma.is_none()
            && self.direct_write.is_none()
            && self.index_pulse_cck == 0
            && self.index_flag_sync_cck == 0
            && self.selected_drive().is_none()
            && self.drives.iter().all(FloppyDrive::is_settled)
    }

    /// Cheap idle test for the per-CPU-access device tick: the cached result
    /// of the last `is_idle()` recompute. Always reflects current state because
    /// every activation path clears it and `tick` recomputes it.
    pub fn is_idle_cached(&self) -> bool {
        self.idle_cache
    }

    pub fn tick(&mut self, cck: u32, dmacon: u16, chip_ram: &mut [u8]) -> bool {
        self.dma_trace.clear();
        self.idle_cache = self.is_idle();
        if self.idle_cache {
            return false;
        }
        self.tick_index_pulse(cck);
        let active_dma = self
            .dma
            .as_ref()
            .filter(|_| self.dma_enabled(dmacon))
            .map(|dma| (dma.drive, dma.track, dma.write));
        let selected_drive = self.selected_drive();
        if let Some((idx, track, _)) = active_dma {
            self.ensure_track(idx, track);
        }
        if let Some(idx) = selected_drive {
            self.ensure_track(idx, self.track_for_drive(idx));
        }
        for drive in self.drives.iter_mut() {
            drive.tick_motor(cck);
        }

        // The reading drive feeds Paula's read shifter and (when selected)
        // emits index pulses. Prefer the active-DMA drive, else the selected.
        let Some(idx) = active_dma.map(|(drive, _, _)| drive).or(selected_drive) else {
            return false;
        };
        let is_selected = selected_drive == Some(idx);

        // Speed >100% overclocks the data path: the platter (and with it the
        // shifter, sync detection, DSKBYTR, and DMA pacing) sees a whole
        // multiple of the elapsed time, leaving every per-cell decision
        // bit-identical to real speed. Mechanics (motor, seek, settle, index
        // pulse width) above tick at real time regardless. A bridged bay is
        // excluded: its data rate belongs to the real disk.
        let data_scale = self.drive_speed_multiplier(idx);
        let data_cck = cck.saturating_mul(data_scale);
        let mut irq = match active_dma {
            Some((dma_idx, _, true)) if dma_idx == idx => {
                self.tick_write_dma(idx, data_cck, data_scale, is_selected, chip_ram, false)
            }
            _ => self.tick_read_and_rotate(
                idx,
                data_cck,
                data_scale,
                dmacon,
                is_selected,
                chip_ram,
                false,
            ),
        };
        // Turbo remains a zero-emulated-time operation while tracing. Its
        // transfers are tagged below so the bus trace can retain all of them
        // in a separate ordered list at the burst's beam position.
        if self.turbo() {
            irq |= self.turbo_burst(cck, dmacon, chip_ram);
        }
        irq
    }

    /// Turbo: spin the platter forward far enough, in zero emulated time, to
    /// complete the pending DMA through the ordinary bit engine -- first to
    /// the next DSKSYNC match when the read is sync-waiting, then to the
    /// transfer's end. Everything (shifter framing, sync realign, the write
    /// path, DSKBLK) behaves exactly as if the time had really passed; only
    /// the machine's clock does not advance. Mirrors FS-UAE's turbo, which
    /// completes the transfer inside the DSKLEN write: here the deferred
    /// `TURBO_DMA_GRACE_CCK` window stands in for its two-scanline delay.
    /// A sync word that never matches leaves the DMA to normal pacing, like
    /// FS-UAE falling back to its bit engine.
    fn turbo_burst(&mut self, cck: u32, dmacon: u16, chip_ram: &mut [u8]) -> bool {
        if self.turbo_grace_cck > 0 {
            self.turbo_grace_cck = self.turbo_grace_cck.saturating_sub(cck);
            if self.turbo_grace_cck > 0 {
                return false;
            }
        }
        if self.turbo_burst_spent {
            return false;
        }
        let Some(dma) = self.dma.as_ref() else {
            return false;
        };
        if !self.dma_enabled(dmacon) {
            return false;
        }
        let (idx, write) = (dma.drive, dma.write);
        // A physical platter cannot be spun forward in zero time: a burst
        // against a bridged bay would hand the guest cells the drive has not
        // delivered. That transfer stays on real-time pacing.
        if self.drives[idx].is_bridged() {
            return false;
        }
        // Mechanics stay honest: a drive that is still spinning up or whose
        // head is settling after a step delivers no stable cells, so the
        // burst waits for it like real-time pacing would.
        if !self.drives[idx].ready() || self.drives[idx].seek_settle_cck > 0 {
            return false;
        }
        let is_selected = self.selected_drive() == Some(idx);
        let mut irq = false;
        // Two phases at most (reach sync, then drain the transfer), plus one
        // spare for per-word framing rounding.
        for _ in 0..3 {
            let Some(dma) = self.dma.as_ref() else { break };
            // Slack above the exact prediction absorbs sub-word framing
            // phase; the engine idles through any excess.
            let burst = if dma.wait_sync {
                let drive = &self.drives[idx];
                let Some(rev) = drive.cur_rev() else { break };
                let Some(bits) = rev.bits_until_sync(drive.rotation_bit, self.dsksync) else {
                    break;
                };
                drive.head_cck_for_bits(bits.max(1)) + 32
            } else if let Some(pred) = self.next_completion_cck_raw(dmacon) {
                u64::from(pred) + 32
            } else {
                break;
            };
            let burst = burst.min(u64::from(u32::MAX)) as u32;
            irq |= if write {
                self.tick_write_dma(idx, burst, 1, is_selected, chip_ram, true)
            } else {
                self.tick_read_and_rotate(idx, burst, 1, dmacon, is_selected, chip_ram, true)
            };
            if irq {
                break;
            }
        }
        // A transfer still pending got its one attempt; leave it to normal
        // pacing until the next DSKLEN arming.
        self.turbo_burst_spent = self.dma.is_some();
        irq
    }

    /// Advance the reading drive's head one MFM cell at a time at the recovered
    /// per-cell rate, feeding each cell to Paula's read shifter. Handles the
    /// live read DMA (bit-aligned sync wait, sync-framed word transfer) and the
    /// free-running sync comparator / DSKSYNC interrupt.
    fn tick_read_and_rotate(
        &mut self,
        idx: usize,
        cck: u32,
        data_scale: u32,
        dmacon: u16,
        is_selected: bool,
        chip_ram: &mut [u8],
        instantaneous: bool,
    ) -> bool {
        // A bridged bay retains a filler track while its physical drive is
        // empty. No media still means no cells reach Paula, regardless of
        // what is cached for the bridge transport.
        if !self.drive_can_transfer_cells(idx) {
            return false;
        }
        // Free-running comparator mode comes from the mirrored ADKCON; an
        // active read DMA carries its own MSB-sync gate captured at start.
        let free_run_sync = self.adkcon & ADK_WORDSYNC != 0 && self.adkcon & ADK_MSBSYNC == 0;
        let dsksync = self.dsksync;

        let mut read_dma = if self.dma_enabled(dmacon)
            && self
                .dma
                .as_ref()
                .is_some_and(|d| d.drive == idx && !d.write)
        {
            self.dma.take()
        } else {
            None
        };
        let dma_sync_enabled = read_dma.as_ref().is_some_and(|d| !d.msb_sync);
        let sync_enabled = if read_dma.is_some() {
            dma_sync_enabled
        } else {
            free_run_sync
        };

        let mut irq = false;
        let mut index_pulse = false;
        // While the head is still settling after a step it is over garbage, so
        // the platter spins (rotation + index pulses advance, adding latency)
        // but no valid cell reaches the read shifter -- a read issued straight
        // after a seek waits out the settle, then resumes at a rotated position.
        let seeking = self.drives[idx].seek_settle_cck > 0;
        self.drives[idx].rotation_acc_cck = self.drives[idx].rotation_acc_cck.saturating_add(cck);
        'outer: loop {
            if self.drives[idx].cur_rev().is_none() {
                break;
            }
            let cell = self.drives[idx].head_cell_cck();
            if self.drives[idx].rotation_acc_cck < cell {
                break;
            }
            self.drives[idx].rotation_acc_cck -= cell;
            if seeking {
                // Advance the platter (and index) but recover no data.
                if self.drives[idx].advance_head_bit() {
                    index_pulse = true;
                }
                continue;
            }
            let bit = self.drives[idx].head_bit();
            let storing = read_dma.as_ref().is_some_and(|d| !d.wait_sync);
            self.read_shifter.sample_bit(bit, dsksync, storing);

            // The DSKSYN interrupt is edge-triggered, but the comparator
            // itself answers on every cell: a DSKSYNC that a same-bit run
            // keeps matching (0x0000, 0xFFFF) holds the match for the whole
            // run.
            let comparator_match = self.read_shifter.sync_matched();
            if self.read_shifter.take_sync_irq() && sync_enabled {
                self.record_sync_match();
                if let Some(dma) = read_dma.as_mut() {
                    if dma.wait_sync {
                        dma.wait_sync = false;
                        self.read_shifter.realign();
                        // A zero-length read finishes the instant it syncs.
                        if dma.remaining == 0 {
                            irq = true;
                            break 'outer;
                        }
                    }
                }
            }
            if comparator_match && sync_enabled && self.adkcon & ADK_WORDSYNC != 0 {
                // With WORDSYNC set, Paula re-frames the word boundary on
                // every DSKSYNC match, not only on the one that starts a
                // transfer. A revolution whose cell count is not a multiple
                // of 16 otherwise leaves every sector after the index wrap
                // off the word grid, and a reader that scans its buffer on
                // that grid (AROS trackdisk.device) never finds them.
                // Re-framing on every matching cell of a run parks the
                // framing at the run's end, where the first word after it
                // starts. The same bit counter frames DSKBYTR, so the reset
                // applies with no transfer running too: a reader polling
                // bytes after WORDEQUAL gets them framed from the sync. (A
                // transfer that was waiting for this sync already realigned
                // on the edge above; re-framing again is idempotent.)
                self.read_shifter.reframe();
            }

            if self.drives[idx].advance_head_bit() {
                index_pulse = true;
            }

            if let Some(dma) = read_dma.as_mut() {
                if !dma.wait_sync {
                    while let Some(word) = self.read_shifter.read_fifo_word() {
                        if dma.remaining == 0 {
                            break;
                        }
                        let addr = self.dskpt;
                        write_chip_word(chip_ram, addr, word);
                        if self.trace_dma_enabled {
                            self.dma_trace.push(DiskDmaTransfer {
                                addr,
                                data: word,
                                memory_write: true,
                                cck_offset: dma_trace_cck_offset(
                                    cck,
                                    self.drives[idx].rotation_acc_cck,
                                    data_scale,
                                ),
                                instantaneous,
                            });
                        }
                        if !self.debug_watch_addrs.is_empty()
                            && self.debug_watch_addrs.contains(&(self.dskpt & 0x00FF_FFFE))
                        {
                            self.debug_watched_write = Some((self.dskpt, word));
                        }
                        self.last_dskdatr = word;
                        self.advance_dskpt();
                        dma.remaining -= 1;
                        if dma.remaining == 0 {
                            irq = true;
                            break 'outer;
                        }
                    }
                }
            }
        }

        if index_pulse && is_selected {
            self.start_index_pulse();
        }
        if let Some(dma) = read_dma {
            if irq {
                self.finish_dma(dma);
            } else {
                self.dma = Some(dma);
            }
        }
        irq
    }

    /// Word-paced write DMA: capture CPU words from chip RAM and advance the
    /// head one 16-cell word per word_cck. The captured stream is decoded back
    /// to sectors / raw MFM and persisted when the DMA finishes.
    fn tick_write_dma(
        &mut self,
        idx: usize,
        cck: u32,
        data_scale: u32,
        is_selected: bool,
        chip_ram: &mut [u8],
        instantaneous: bool,
    ) -> bool {
        // Match the read path: a bridge filler track must not make an empty
        // physical drive accept a write.
        if !self.drive_can_transfer_cells(idx) {
            return false;
        }
        let Some(mut dma) = self.dma.take() else {
            return false;
        };
        let mut irq = false;
        let mut index_pulse = false;
        self.drives[idx].rotation_acc_cck = self.drives[idx].rotation_acc_cck.saturating_add(cck);
        loop {
            if dma.remaining == 0 {
                irq = true;
                break;
            }
            let word_cck = self.drives[idx].head_word_cck();
            if self.drives[idx].rotation_acc_cck < word_cck {
                break;
            }
            self.drives[idx].rotation_acc_cck -= word_cck;
            if dma.write_start_pending {
                // A start position sampled while the mechanism was idle is
                // not meaningful (inserting media resets rotation). Latch
                // the actual head position when the first word is consumed.
                dma.write_start_pending = false;
                dma.write_start_word = self.drives[idx].rotation_bit / 16;
                dma.write_start_bit = (self.drives[idx].rotation_bit % 16) as u8;
            }
            let addr = self.dskpt;
            let word = read_chip_word(chip_ram, addr);
            if self.trace_dma_enabled {
                self.dma_trace.push(DiskDmaTransfer {
                    addr,
                    data: word,
                    memory_write: false,
                    cck_offset: dma_trace_cck_offset(
                        cck,
                        self.drives[idx].rotation_acc_cck,
                        data_scale,
                    ),
                    instantaneous,
                });
            }
            dma.write_words.push(word);
            self.advance_dskpt();
            for _ in 0..16 {
                if self.drives[idx].advance_head_bit() {
                    index_pulse = true;
                }
            }
            // The shifter is Paula's output path during a write: DSKBYT
            // still pulses as it clocks bytes out, but the byte register
            // holds nothing readable.
            self.last_dskbytr_byte = 0;
            self.dskbyte_valid = true;
            dma.remaining -= 1;
            if dma.remaining == 0 {
                irq = true;
                break;
            }
        }
        if index_pulse && is_selected {
            self.start_index_pulse();
        }
        if irq {
            self.finish_dma(dma);
        } else {
            self.dma = Some(dma);
        }
        irq
    }

    pub fn take_index_pulse(&mut self) -> bool {
        std::mem::take(&mut self.index_flag_ready)
    }

    #[cfg(test)]
    fn index_pulse_active(&self) -> bool {
        self.index_pulse_cck != 0
    }

    pub fn take_sync_irq(&mut self) -> bool {
        std::mem::take(&mut self.sync_irq_latch)
    }

    /// Scale a raw event-horizon prediction for `drive_idx` to the
    /// configured drive speed: at percentage speeds the events land a whole
    /// multiple sooner, and a turbo burst completes them within the grace
    /// window. Predictions only cap the idle fast-forward, so an
    /// under-estimate is always safe -- but a 1-cck prediction the burst
    /// cannot actually honour (drive spinning up, head settling, attempt
    /// already spent) would pin STOP-state fast-forward to single steps for
    /// the whole spin-up, so those cases keep the normal-paced deadline.
    fn scale_prediction(&self, drive_idx: usize, cck: u32) -> u32 {
        if self.drive_turbo(drive_idx) {
            if self.turbo_grace_cck > 0 {
                return cck.min(self.turbo_grace_cck).max(1);
            }
            let drive = &self.drives[drive_idx];
            if !self.turbo_burst_spent && drive.ready() && drive.seek_settle_cck == 0 {
                return 1;
            }
            return cck.max(1);
        }
        (cck / self.drive_speed_multiplier(drive_idx)).max(1)
    }

    pub fn next_completion_cck(&self, dmacon: u16) -> Option<u32> {
        let drive_idx = self.dma.as_ref()?.drive;
        self.next_completion_cck_raw(dmacon)
            .map(|cck| self.scale_prediction(drive_idx, cck))
    }

    fn next_completion_cck_raw(&self, dmacon: u16) -> Option<u32> {
        let dma = self.dma.as_ref()?;
        if !self.dma_enabled(dmacon) || dma.wait_sync || !self.drive_can_transfer_cells(dma.drive) {
            return None;
        }
        let drive = &self.drives[dma.drive];
        let cck = if dma.write {
            // Writes are word-paced: one word per word_cck.
            (dma.remaining as u64)
                .saturating_mul(drive.head_word_cck() as u64)
                .saturating_sub(drive.rotation_acc_cck as u64)
        } else {
            // Reads complete when the shifter frames `remaining` more words.
            // It is already `framing_bits` cells into the current word.
            let bits = (dma.remaining as usize)
                .saturating_mul(16)
                .saturating_sub(self.read_shifter.framing_bits());
            drive.head_cck_for_bits(bits.max(1))
        };
        Some((cck.min(u64::from(u32::MAX)) as u32).max(1))
    }

    /// Whether the active track can currently deliver cells to Paula. Keep
    /// the completion predictor on the same boundary as the read/write
    /// engines: an armed transfer over an idle or empty mechanism remains
    /// pending, but has no completion event for STOP-state pacing to chase.
    fn drive_can_transfer_cells(&self, drive_idx: usize) -> bool {
        let drive = &self.drives[drive_idx];
        drive.motor_on && drive.has_media() && !drive.cached.is_empty()
    }

    pub fn next_sync_irq_cck(&self, dmacon: u16) -> Option<u32> {
        if self.sync_irq_latch {
            return Some(1);
        }
        let dma = self.dma.as_ref()?;
        if dma.write || dma.msb_sync || !self.dma_enabled(dmacon) {
            return None;
        }
        let drive = &self.drives[dma.drive];
        if drive.cached_track != Some(dma.track) || drive.cached.is_empty() {
            return None;
        }
        let rev = drive.cur_rev()?;
        let bits = rev.bits_until_sync(drive.rotation_bit, self.dsksync)?;
        let raw = (drive.head_cck_for_bits(bits).min(u64::from(u32::MAX)) as u32).max(1);
        Some(self.scale_prediction(dma.drive, raw))
    }

    pub fn next_index_pulse_cck(&self) -> Option<u32> {
        if self.index_flag_sync_cck != 0 {
            return Some(self.index_flag_sync_cck);
        }
        let idx = self.selected_drive()?;
        let drive = &self.drives[idx];
        if !drive.has_media() || !drive.motor_on {
            return None;
        }
        let rev = drive.cur_rev()?;
        // A single-word (or shorter) track is too short to advertise an index.
        if rev.bit_len <= 16 {
            return None;
        }
        let bits_to_end = rev.bit_len - (drive.rotation_bit % rev.bit_len);
        let raw = (drive
            .head_cck_for_bits(bits_to_end)
            .min(u64::from(u32::MAX)) as u32)
            .max(1);
        // The platter spins at the data-rate multiple; turbo bursts do not
        // move the index prediction (they are bounded by the completion
        // prediction above, which already caps the fast-forward).
        Some((raw / self.drive_speed_multiplier(idx)).max(1))
    }

    pub fn dma_active(&self, dmacon: u16) -> bool {
        self.dma_enabled(dmacon)
    }

    /// Drain the head-step pulses accumulated since the last call,
    /// for the synthesized drive sound effects.
    pub fn take_sound_steps(&mut self) -> u32 {
        std::mem::take(&mut self.sound_steps)
    }

    /// Drains the media changes bridged bays have noticed: `(bay, present,
    /// tab)`, oldest first, where `tab` is the inserted disk's write-protect
    /// tab on the one kind of drive where it decides anything -- one the
    /// configuration lets write. The frontend raises the same on-screen
    /// message an image insert or eject shows; the log line has already said
    /// it in full.
    #[cfg(feature = "fluxbridge")]
    pub fn take_bridge_media_events(&mut self) -> Vec<(usize, bool, Option<bool>)> {
        std::mem::take(&mut self.bridge_media_events)
    }

    /// Per-drive platter spin level for the drive sound effects: 0.0
    /// stopped to 1.0 at full speed. Rides the motor spin-up/spin-down
    /// accumulator, so the audible motor glides over the real ~0.5 s
    /// ramp instead of switching.
    pub fn motor_spin_levels(&self) -> [f32; 4] {
        std::array::from_fn(|idx| {
            // Silent for a bridged drive: the real platter is spinning in the
            // room, so a synthesized motor on top would double it.
            if self.drives[idx].is_bridged() {
                return 0.0;
            }
            self.drives[idx].motor_cck.min(MOTOR_READY_CCK) as f32 / MOTOR_READY_CCK as f32
        })
    }

    pub fn dskpt(&self) -> u32 {
        self.dskpt
    }

    fn start_dma(&mut self, val: u16, adkcon: u16) -> bool {
        let write = val & DSKLEN_WRITE != 0;
        let remaining = (val & DSKLEN_MASK) as u32;
        if remaining == 0 {
            self.dsklen &= !DSKLEN_DMAEN;
            return true;
        }
        if let Some(direct) = self.direct_write.take() {
            self.finish_direct_write(direct);
        }

        let Some(idx) = self.selected_drive() else {
            if crate::envcfg::flag("COPPERLINE_DIAG_DISK") {
                log::info!("disk-dma refused: no drive selected (prb={:02X})", self.prb);
            }
            return self.no_drive_completion();
        };
        // Paula's disk state machine does not sense drive readiness: DSKLEN
        // arming enters the transfer state regardless, and data starts
        // flowing once the mechanism delivers stable cells. A drive that is
        // still spinning up must therefore arm normally instead of faking an
        // instant, dataless completion. A trackloader may arm within the
        // spin-up window and poll its buffer for the data that arrives later.
        //
        // The same honesty covers a drive with no media or a stopped
        // platter: the transfer arms and then idles, because no cells pass
        // under the head for the shifter to drain. Real Paula waits for
        // sync forever in that state; the guest's own timeout governs, and
        // a media insert or motor start mid-transfer brings the transfer to
        // life exactly as on hardware. The read and write tick paths idle
        // without motor, media, and cached cells, and the turbo burst refuses
        // drives that are not ready, so nothing completes early.
        if (!self.drives[idx].has_media() || !self.drives[idx].motor_on)
            && crate::envcfg::flag("COPPERLINE_DIAG_DISK")
        {
            log::info!(
                "disk-dma armed against an idle mechanism: df{idx} media={} motor_on={} \
                 motor_cck={} (transfer will pend until the mechanism delivers)",
                self.drives[idx].has_media(),
                self.drives[idx].motor_on,
                self.drives[idx].motor_cck,
            );
        }

        let track = self.track_for_drive(idx);
        self.ensure_track(idx, track);
        let word_sync = !write && (adkcon & ADK_WORDSYNC != 0);
        let msb_sync = !write && (adkcon & ADK_MSBSYNC != 0);
        let (write_start_word, write_start_bit) = if write {
            (
                self.drives[idx].rotation_bit / 16,
                (self.drives[idx].rotation_bit % 16) as u8,
            )
        } else {
            // A read drains Paula's live serial-to-parallel shifter on its
            // recovered disk word phase. If it waits for sync, framing
            // realigns to the sync bit phase when it locks.
            self.read_shifter
                .reset_framing_to_phase((self.drives[idx].rotation_bit % 16) as u8);
            (0, 0)
        };
        self.dma = Some(DiskDma {
            drive: idx,
            track,
            write,
            remaining,
            wait_sync: word_sync,
            msb_sync,
            write_words: Vec::new(),
            write_start_word,
            write_start_bit,
            write_start_pending: write
                && (!self.drives[idx].has_media() || !self.drives[idx].motor_on),
        });
        if self.turbo() {
            self.turbo_grace_cck = TURBO_DMA_GRACE_CCK;
            self.turbo_burst_spent = false;
        }
        debug!(
            "floppy DMA start df{} track={} write={} words={} sync_wait={} msb_sync={}",
            idx, track, write, remaining, word_sync, msb_sync
        );
        if crate::envcfg::flag("COPPERLINE_DIAG_DISK") {
            let secs = self.drives[idx].elapsed_cck as f64 / PAULA_CLOCK_HZ as f64;
            log::info!(
                "disk-dma secs={secs:.5} df{idx} track={track} cyl={} write={write} words={remaining} rotbit={} cached_track={:?} revs={} rev0_words={} settle={}",
                self.drives[idx].cylinder,
                self.drives[idx].rotation_bit,
                self.drives[idx].cached_track,
                self.drives[idx].cached.revs.len(),
                self.drives[idx]
                    .cached
                    .revs
                    .first()
                    .map(|rev| rev.words.len())
                    .unwrap_or(0),
                self.drives[idx].seek_settle_cck,
            );
        }
        false
    }

    fn no_drive_completion(&mut self) -> bool {
        self.dsklen &= !DSKLEN_DMAEN;
        true
    }

    fn handle_active_dma_track_change(&mut self, drive_idx: usize) {
        let Some(mut dma) = self.dma.take() else {
            return;
        };
        if dma.drive != drive_idx {
            self.dma = Some(dma);
            return;
        }

        let new_track = self.track_for_drive(drive_idx);
        if dma.track == new_track {
            self.dma = Some(dma);
            return;
        }

        if dma.write && !dma.write_words.is_empty() {
            self.finish_write_words(
                dma.drive,
                dma.track,
                &dma.write_words,
                dma.write_start_word,
                dma.write_start_bit,
                false,
            );
            dma.write_words.clear();
        }

        self.ensure_track(drive_idx, new_track);
        dma.track = new_track;
        dma.write_start_word = self.drives[drive_idx].rotation_bit / 16;
        dma.write_start_bit = (self.drives[drive_idx].rotation_bit % 16) as u8;
        dma.write_start_pending =
            !self.drives[drive_idx].has_media() || !self.drives[drive_idx].motor_on;
        self.dma = Some(dma);
    }

    fn start_index_pulse(&mut self) {
        self.index_pulse_cck = INDEX_PULSE_CCK;
        self.index_flag_sync_cck = INDEX_FLAG_SYNC_CCK;
    }

    fn tick_index_pulse(&mut self, cck: u32) {
        let previous_sync = self.index_flag_sync_cck;
        self.index_flag_sync_cck = self.index_flag_sync_cck.saturating_sub(cck);
        if previous_sync != 0 && self.index_flag_sync_cck == 0 {
            self.index_flag_ready = true;
        }
        self.index_pulse_cck = self.index_pulse_cck.saturating_sub(cck);
    }

    fn finish_dma(&mut self, dma: DiskDma) {
        if crate::envcfg::flag("COPPERLINE_DIAG_DISK") {
            log::info!(
                "disk-dma finish df{} track={} write={} remaining={} wait_sync={} dskpt={:#08X}",
                dma.drive,
                dma.track,
                dma.write,
                dma.remaining,
                dma.wait_sync,
                self.dskpt,
            );
        }
        self.dsklen &= !DSKLEN_DMAEN;
        if dma.write && !dma.write_words.is_empty() {
            self.finish_write_dma(dma);
        }
    }

    fn finish_write_dma(&mut self, dma: DiskDma) {
        self.finish_write_words(
            dma.drive,
            dma.track,
            &dma.write_words,
            dma.write_start_word,
            dma.write_start_bit,
            true,
        );
    }

    fn finish_direct_write(&mut self, direct: DiskDirectWrite) {
        self.finish_write_words(
            direct.drive,
            direct.track,
            &direct.write_words,
            direct.write_start_word,
            direct.write_start_bit,
            true,
        );
    }

    fn finish_write_words(
        &mut self,
        drive_idx: usize,
        track: usize,
        write_words: &[u16],
        write_start_word: usize,
        write_start_bit: u8,
        lose_tail_bits: bool,
    ) {
        if write_words.is_empty() {
            return;
        }
        let drive = &mut self.drives[drive_idx];

        // A bridged drive writes the MFM straight back to the real disk, laid
        // down from the head position the guest started writing at -- the same
        // place a real drive would have begun putting cells on the platter.
        // Read before the drive is borrowed to write through, so the guard
        // below and the /WPRO line the guest was given are the same two facts.
        #[cfg(feature = "fluxbridge")]
        let (config_protected, tab_protected) = (
            drive.bridge_write_protected,
            drive.bridge_tab_write_protected,
        );
        #[cfg(feature = "fluxbridge")]
        if let Some(bridge) = drive.bridge.as_mut() {
            // Two separate protections, and this is the last point either can
            // stop physical media being written. The emulated /WPRO line has
            // already told the guest not to try -- but a program that writes
            // anyway, or a guest that ignores the line, must not reach the
            // platter, so both are checked here rather than trusted to the
            // machine above.
            //
            // Deliberately the same two the /WPRO line is built from, rather
            // than asking the drive again: a second reading taken here can
            // disagree with the one the guest was given, and then a disk the
            // machine calls protected is the one being written to.
            if config_protected {
                warn!(
                    "floppy.df{drive_idx} write ignored: this drive is write-protected in the \
                     configuration (write_protected = false to allow writing to a real disk)"
                );
                return;
            }
            if tab_protected {
                warn!("floppy.df{drive_idx} write ignored: the disk's own tab is closed");
                return;
            }
            let cylinder = (track / SIDES) as u8;
            let side = !track.is_multiple_of(SIDES);
            let start_bit = write_start_word * 16 + write_start_bit as usize;
            if bridge.write_track(cylinder, side, write_words, start_bit) {
                if bridge_diag() {
                    info!(
                        "fluxbridge.df{drive_idx} track {track} (cyl {cylinder} side {}) \
                         written: {} words from bit {start_bit}, queued to the platter",
                        u8::from(side),
                        write_words.len(),
                    );
                } else {
                    debug!(
                        "floppy.df{drive_idx} bridge wrote {} words to track {track} \
                         at bit {start_bit}",
                        write_words.len()
                    );
                }
                // Accepted, not yet on the platter. Hold the track until the
                // drive says what became of it: until then the library still
                // has the recording from before the write, and serving that
                // would show the guest a disk that no longer exists.
                if !drive.bridge_writing.contains(&track) {
                    drive.bridge_writing.push(track);
                }
            } else {
                warn!("floppy.df{drive_idx} bridge write of track {track} was rejected");
            }
            // Re-read on the next access: what is on the platter now is
            // whatever the drive actually managed to lay down, which is not
            // necessarily what was asked for.
            drive.cached_track = None;
            drive.bridge_partial_track = None;
            if let Some(known) = drive.bridge_tracks.get_mut(track) {
                *known = BridgeTrack::Unknown;
            }
            return;
        }

        let Some(image) = drive.image.as_mut() else {
            return;
        };
        if image.write_protected {
            warn!(
                "floppy.df{} write ignored: image is write-protected",
                drive_idx
            );
            return;
        }
        let path = image.path.clone();
        let legacy_extended_adf = image.legacy_extended_adf;
        let backing = image.backing;
        let write_result: Result<()> = match &mut image.data {
            FloppyImageData::StandardAdf(image_data) => {
                decode_non_empty_track_write(track, write_words).and_then(|sectors| {
                    apply_standard_adf_sectors(image_data, track, &sectors);
                    match backing {
                        FloppyImageBacking::File => {
                            write_standard_adf_sectors_to_disk(&path, track, &sectors)
                        }
                        FloppyImageBacking::Memory => Ok(()),
                    }
                })
            }
            FloppyImageData::Tracks(tracks) => apply_extended_adf_write(
                tracks,
                track,
                write_words,
                write_start_word,
                write_start_bit,
                legacy_extended_adf,
                lose_tail_bits,
            )
            .and_then(|()| match backing {
                FloppyImageBacking::File => {
                    let encoded = if legacy_extended_adf {
                        encode_uae_legacy_extended_adf(tracks)
                    } else {
                        encode_uae_extended_adf(tracks)
                    }?;
                    std::fs::write(&path, encoded).context("writing extended ADF image")
                }
                FloppyImageBacking::Memory => Ok(()),
            }),
        };
        match write_result {
            Ok(()) => {
                drive.cached_track = None;
                debug!("floppy.df{} image write complete", drive_idx);
            }
            Err(e) => warn!("floppy.df{} image write failed: {e:#}", drive_idx),
        }
    }

    fn selected_ready_track(&self) -> Option<(usize, usize)> {
        let idx = self.selected_drive()?;
        if !self.drives[idx].ready() {
            return None;
        }
        Some((idx, self.track_for_drive(idx)))
    }

    fn selected_drive(&self) -> Option<usize> {
        CIAB_DSKSEL_MASKS
            .iter()
            .enumerate()
            .find(|(idx, select_mask)| self.drive_connected(*idx) && self.prb & **select_mask == 0)
            .map(|(idx, _)| idx)
    }

    fn track_for_drive(&self, idx: usize) -> usize {
        self.drives[idx].cylinder as usize * SIDES + self.side
    }

    /// The 16-bit MFM word currently under the head (bit-aligned at the head
    /// position), for DSKDATR.
    fn peek_head_word(&self, idx: usize) -> Option<u16> {
        let drive = self.drives.get(idx)?;
        let rev = drive.cur_rev()?;
        Some(rev.word_at((drive.rotation_bit / 16) * 16))
    }

    /// Test helper: read the 16-bit word at the head and advance the head one
    /// word (16 cells), firing the index pulse when it wraps a revolution.
    #[cfg(test)]
    fn next_disk_word(&mut self, idx: usize, track: usize) -> Option<u16> {
        self.ensure_track(idx, track);
        let word = self.peek_head_word(idx)?;
        let mut index = false;
        for _ in 0..16 {
            if self.drives[idx].advance_head_bit() {
                index = true;
            }
        }
        if index {
            self.start_index_pulse();
        }
        Some(word)
    }

    fn current_disk_word_matches_sync(&mut self) -> bool {
        let Some((idx, track)) = self.selected_ready_track() else {
            return false;
        };
        self.ensure_track(idx, track);
        self.peek_head_word(idx)
            .is_some_and(|word| word == self.dsksync)
    }

    /// Test helper: put the head on a word boundary with a fresh read
    /// shifter, so the shifter's framing follows the track's word grid from
    /// here and the cells it samples are exactly the ones ticked from now.
    #[cfg(test)]
    fn park_head_at_word(&mut self, idx: usize, word: usize) {
        self.drives[idx].set_rotation_word(word);
        self.drives[idx].rotation_acc_cck = 0;
        self.read_shifter = PaulaDiskReadDpllFifo::new();
    }

    /// Test helper: advance the platter exactly `cells` MFM cells, one tick
    /// per cell, returning whether any tick raised DSKBLK.
    #[cfg(test)]
    fn tick_cells(&mut self, idx: usize, cells: usize, dmacon: u16) -> bool {
        let mut irq = false;
        for _ in 0..cells {
            let cell = self.drives[idx].head_cell_cck();
            irq |= self.tick(cell, dmacon, &mut []);
        }
        irq
    }

    fn record_sync_match(&mut self) {
        self.sync_irq_latch = true;
    }

    fn ensure_track(&mut self, idx: usize, track: usize) {
        // A physical drive has one head, and it is where the guest's stepper
        // put it. `tick` asks for the DMA's track as well as the head's, and
        // when a transfer outlives a step those two disagree -- so asking for
        // both drags the real head between them, tick after tick, reading
        // nothing useful either way. Only fetch what the head is over; the
        // transfer gets what is passing under it, as it would on hardware.
        #[cfg(feature = "fluxbridge")]
        if self.drives[idx].is_bridged() && track != self.track_for_drive(idx) {
            return;
        }
        let drive = &mut self.drives[idx];
        // A revolution the head has already been all the way round is spent:
        // a physical one that did not start at the index cannot be turned
        // again, because its two ends are a revolution apart in time and the
        // join between them falls inside a sector. Fall through and fetch the
        // recording that followed it instead.
        #[cfg(feature = "fluxbridge")]
        let spent = drive.bridge_rev_spent;
        #[cfg(not(feature = "fluxbridge"))]
        let spent = false;
        if drive.cached_track == Some(track) && !spent {
            return;
        }
        // Whatever is already turning under the head for this very track
        // stays: filler keeps the platter moving while a capture finishes, and
        // a part-captured revolution is real data the guest is part way
        // through reading. Both are provisional, and both are rebuilt only
        // when they are replaced -- clearing either every tick would leave the
        // head with nothing to read between polls.
        #[cfg(feature = "fluxbridge")]
        let keep_provisional =
            drive.bridge_filler_track == Some(track) || drive.bridge_partial_track == Some(track);
        #[cfg(not(feature = "fluxbridge"))]
        let keep_provisional = false;
        if !keep_provisional {
            drive.cached = CachedTrack::default();
        }

        // A bridged drive reads the track off the real disk instead. The head
        // is already on its way: it followed the emulated stepper when the
        // guest moved it (see `sync_bridge_head`), and the driver names the
        // track it wants again here in case nothing stepped at all.
        #[cfg(feature = "fluxbridge")]
        if drive.bridge.is_some() {
            // A real drive only reads while the platter is turning, and drive
            // select alone gets us here with the motor still off. Leave the
            // track uncached until it is up to speed, which retries.
            if !drive.motor_on || drive.motor_cck < MOTOR_READY_CCK {
                return;
            }
            // Read off this disk before, and proven faithful? Hand back what
            // it said then. The platter cannot have changed underneath
            // without the change line saying so or a write going through, and
            // both empty this, so another rotation would only fetch the same
            // bits again. Only a kept recording qualifies: one that is
            // index-aligned or verified clean, so its two ends genuinely meet
            // and it can turn under the head like an image's track. A spent
            // revolution is the exception even then: handing that back is
            // precisely the same recording over again.
            if let Some(BridgeTrack::Kept(known)) =
                drive.bridge_tracks.get(track).filter(|_| !spent)
            {
                drive.cached = known.clone();
                drive.cached_track = Some(track);
                drive.bridge_rev_seamless = true;
                drive.bridge_filler_track = None;
                drive.bridge_wait_since_cck = 0;
                drive.bridge_attempts = 0;
                drive.clamp_head();
                return;
            }
            // A write of this track is on its way to the platter. The drive
            // still holds the recording taken before it, so reading now would
            // hand the guest the disk as it was -- and if that recording
            // proved out, it would be filed as faithful and served from
            // memory long after the platter had moved on. Wait for the
            // outcome; the guest's own re-read finds the new cells.
            if drive.bridge_writing.contains(&track) {
                drive.bridge_poll_cck = drive.elapsed_cck + BRIDGE_POLL_INTERVAL_CCK;
                return;
            }
            // While the head is still travelling (or ringing after the last
            // step) the guest cannot recover data anyway -- `head_bit` holds
            // read-data back for exactly this window -- so asking the real
            // drive for anything mid-seek only makes it abort a capture it
            // will immediately restart. Stay quiet until the head settles.
            if drive.seek_settle_cck > 0 {
                return;
            }
            // This runs from `tick`, so a track that is not ready would
            // otherwise be asked for millions of times a second. The driver
            // needs a whole revolution -- around 200ms -- to capture one, so
            // pausing a millisecond between attempts costs nothing measurable
            // and leaves the emulated machine to get on with it.
            if drive.elapsed_cck < drive.bridge_poll_cck {
                return;
            }
            let cylinder = (track / SIDES) as u8;
            let side = !track.is_multiple_of(SIDES);
            // Time this track, not "everything since the last success". A seek
            // walks through every cylinder on the way and asks for each in
            // turn, so a timer that only reset on a completed read counted the
            // whole journey against whichever track happened to end it.
            if drive.bridge_wait_since_cck == 0 || drive.bridge_wait_track != Some(track) {
                drive.bridge_wait_since_cck = drive.elapsed_cck.max(1);
                drive.bridge_wait_track = Some(track);
                drive.bridge_attempts = 0;
            }
            drive.bridge_attempts = drive.bridge_attempts.saturating_add(1);
            // Retire the spent recording so the driver promotes the capture
            // that followed it, if one has finished. When none has, it hands
            // the same revolution back, and Copperline uses it: the guest gets
            // a splice at the join and retries the sector that straddles it,
            // which costs a revolution. Refusing it instead was tried and is
            // worse -- the drive cannot finish a capture every revolution, so
            // the guest is handed filler and starves. Ten good sectors beat
            // none. `compatible` is the mode that has no seam to begin with.
            //
            // A return visit to a track whose recording was served but not
            // kept is the same situation: what the driver still holds is the
            // recording the guest already saw (and may have rejected), so
            // retire it too -- the retry deserves the recording that
            // followed, not the one that failed it.
            // Once per wait, not once per poll: repeated switching while the
            // drive is still capturing just cycles the two stale recordings.
            let revisit_unverified = drive.cached_track != Some(track)
                && matches!(
                    drive.bridge_tracks.get(track),
                    Some(BridgeTrack::Unverified)
                );
            if (spent || revisit_unverified) && drive.bridge_attempts == 1 {
                if let Some(bridge) = drive.bridge.as_mut() {
                    bridge.switch_buffer(side);
                }
            }
            // Read before the bridge is borrowed: the serving fit below needs
            // it in both the capture and filler paths.
            let serve_percent = u32::from(drive.bridge_speed_percent.max(100));
            let bridge = drive.bridge.as_mut().expect("checked above");
            if let Some((words, bits)) = bridge.read_track(cylinder, side) {
                // The bridge hands back one whole revolution as a packed MFM
                // stream plus the bit it wrapped at, which is what TrackRev
                // holds. Because it is a real revolution, fitting it to one
                // rotation gives the disk's own data rate: a slightly long or
                // short track -- a drive running off-speed -- stays
                // self-consistent, exactly as a captured image does.
                // `bridge_speed` compresses that fit, serving the same cells
                // in proportionally less time.
                let word_cck =
                    (Self::word_cck_for_track_words(words.len()) * 100 / serve_percent).max(1);
                // Whether this recording can be trusted beyond a single pass
                // is FluxBridge's own verdict: index-aligned captures close
                // on themselves by construction, and an index-less one is
                // kept only when the library proved it -- the join by pattern
                // matching, the decode as a complete AmigaDOS track with
                // every checksum passing. Anything less is served once and
                // fetched afresh next time, so a retry always reaches new
                // flux; formats the library cannot verify are simply never
                // kept in index-less mode, which costs a re-read on revisit
                // and never costs correctness.
                let quality = bridge.last_quality();
                let keep = quality.reusable();
                // How long the drive took, and how many times it had to be
                // asked, is what separates a disk the head cannot read from an
                // interface that is slow: a healthy DD track is one revolution,
                // so about 200ms. Worth an info line when diagnosing a drive,
                // and debug otherwise.
                let waited_ms = drive
                    .elapsed_cck
                    .saturating_sub(drive.bridge_wait_since_cck)
                    * 1000
                    / PAULA_CLOCK_HZ as u64;
                log::log!(
                    if bridge_diag() {
                        log::Level::Info
                    } else {
                        log::Level::Debug
                    },
                    "fluxbridge.df{idx} track {track} (cyl {cylinder} side {}) read: \
                     {bits} bits, {} words, {waited_ms}ms over {} attempt{}, \
                     {word_cck} cck/word, {quality:?}{}",
                    u8::from(side),
                    words.len(),
                    drive.bridge_attempts,
                    if drive.bridge_attempts == 1 { "" } else { "s" },
                    if keep { "" } else { ", serving once" },
                );
                drive.bridge_wait_since_cck = 0;
                drive.bridge_attempts = 0;
                drive.cached.revs = vec![TrackRev::new(words, bits, word_cck)];
                drive.bridge_rev_spent = false;
                drive.bridge_rev_seamless = keep;
                drive.cached_track = Some(track);
                drive.bridge_filler_track = None;
                // The finished revolution supersedes the part-captured one it
                // grew from.
                drive.bridge_partial_track = None;
                if drive.bridge_tracks.len() <= track {
                    drive.bridge_tracks.resize(track + 1, BridgeTrack::Unknown);
                }
                drive.bridge_tracks[track] = if keep {
                    BridgeTrack::Kept(drive.cached.clone())
                } else {
                    BridgeTrack::Unverified
                };
                // A track that read is proof of a disk, whatever the sense line
                // says a moment later.
                drive.bridge_media = true;
                drive.clamp_head();
            } else {
                // Leave the track uncached so the next access tries again. A
                // read comes back empty while the platter is still coming up to
                // speed, or before the driver has captured a revolution --
                // transient states a real drive simply retries through.
                // Caching that would wedge the drive on a disk that is there.
                drive.bridge_poll_cck = drive.elapsed_cck + BRIDGE_POLL_INTERVAL_CCK;
                if bridge_diag() && drive.bridge_attempts == 1 {
                    info!(
                        "fluxbridge.df{idx} waiting for track {track} (cyl {cylinder} side {}) \
                         [ready={} disk={} motor={} at_cyl={}]",
                        u8::from(side),
                        bridge.is_ready(),
                        bridge.disk_in_drive(),
                        bridge.motor_running(),
                        bridge.current_cylinder(),
                    );
                }
                trace!(
                    "floppy.df{idx} bridge: track {track} not ready, will retry \
                     (ready={} disk={} motor={} at_cyl={} want_cyl={} working={})",
                    bridge.is_ready(),
                    bridge.disk_in_drive(),
                    bridge.motor_running(),
                    bridge.current_cylinder(),
                    cylinder,
                    bridge.is_working(),
                );
                // A capture on its way in is better than filler: its early
                // sectors are real, and the guest can read them while the rest
                // of the revolution is still passing the head -- which is
                // exactly how the machine this emulates read its disks. Served
                // at the platter's own pace, never compressed: the head must
                // not outrun the growth edge, and both move at real speed with
                // the head spotted this much of a start.
                if let Some((words, bits)) = bridge.partial_track(cylinder, side) {
                    if bits >= BRIDGE_PARTIAL_MIN_BITS {
                        let nominal = encoded_track_words();
                        drive.cached.revs = vec![TrackRev::new(
                            words,
                            bits,
                            Self::word_cck_for_track_words(nominal),
                        )];
                        drive.bridge_filler_track = None;
                        // Mark what this is standing in for, so the next tick
                        // keeps it rather than clearing the cache and leaving
                        // the head with nothing until the following poll.
                        drive.bridge_partial_track = Some(track);
                        drive.bridge_rev_seamless = false;
                        // Refresh the growth at the poll cadence, not every
                        // tick: the snapshot only changes when the worker
                        // publishes, and re-cloning it thousands of times a
                        // second would tax the emulation thread for nothing.
                        drive.bridge_poll_cck = drive.elapsed_cck + BRIDGE_POLL_INTERVAL_CCK;
                        drive.clamp_head();
                        return;
                    }
                }
                // Keep the head over something. Stopping the platter until the
                // capture lands makes the guest pay the capture and then its
                // own rotational wait one after the other; turning over filler
                // means the two overlap, which is how a drive that has data
                // the moment it arrives behaves.
                if drive.bridge_filler_track != Some(track) {
                    let nominal = encoded_track_words();
                    drive.cached.revs = vec![TrackRev::filler(
                        nominal * 16,
                        (Self::word_cck_for_track_words(nominal) * 100 / serve_percent).max(1),
                    )];
                    drive.bridge_filler_track = Some(track);
                    drive.bridge_rev_seamless = false;
                    drive.clamp_head();
                }
            }
            return;
        }

        if let Some(image) = drive.image.as_ref() {
            if let Some(stream) = image.track_stream(track) {
                drive.cached.revs = stream.revs;
            }
            drive.cached_track = Some(track);
            drive.clamp_head();
        }
    }

    fn dma_enabled(&self, dmacon: u16) -> bool {
        self.dma.is_some()
            && self.dsklen & DSKLEN_DMAEN != 0
            && dmacon & DMACON_DMAEN != 0
            && dmacon & DMACON_DISK != 0
    }

    fn advance_dskpt(&mut self) {
        self.dskpt = self.dskpt.wrapping_add(2) & self.dma_ptr_mask();
    }

    fn dma_ptr_mask(&self) -> u32 {
        let mask = if self.dma_addr_mask == 0 {
            0x001F_FFFF
        } else {
            self.dma_addr_mask
        };
        mask & !1
    }

    fn word_cck_for_track_words(words: usize) -> u32 {
        let words = words.max(1) as u32;
        (PAULA_CLOCK_HZ / ROTATION_HZ / words).max(1)
    }

    #[cfg(test)]
    fn word_cck(&self) -> u32 {
        Self::word_cck_for_track_words(encoded_track_words())
    }
}

#[derive(serde::Serialize, serde::Deserialize)]
struct FloppyDrive {
    image: Option<FloppyImage>,
    /// A real drive standing in for the image through whichever FluxBridge
    /// driver this build enables (Greaseweazle in the standard build).
    /// Mutually exclusive with `image`: whichever is present supplies the
    /// track under the head.
    ///
    /// Skipped by the save-state serialiser because a physical disk cannot be
    /// snapshotted -- a state saved with a bridge attached reloads as an empty
    /// drive, which is also why a bridged run is not reproducible.
    #[cfg(feature = "fluxbridge")]
    #[serde(skip)]
    bridge: Option<crate::fluxbridge::Bridge>,
    /// Last known answer to "is there a disk in the real drive", refreshed
    /// deliberately rather than polled per status read (see `has_media`).
    #[cfg(feature = "fluxbridge")]
    #[serde(skip)]
    bridge_media: bool,
    /// The config's own write protection for a bridged drive, held so the
    /// live tab state can be re-combined with it as the drive is polled.
    #[cfg(feature = "fluxbridge")]
    #[serde(skip)]
    bridge_write_protected: bool,
    /// The disk's own write-protect tab, refreshed each time the drive is
    /// polled. The driver keeps the last reading and hands it back whatever
    /// the motor is doing, so this is good with the platter stopped -- which
    /// is most of the time. Both the /WPRO line the guest sees and the guard
    /// on the write itself come from here, so the two cannot disagree.
    #[cfg(feature = "fluxbridge")]
    #[serde(skip)]
    bridge_tab_write_protected: bool,
    /// `elapsed_cck` before which not to ask the bridge for a track again,
    /// after it said it had none ready.
    #[cfg(feature = "fluxbridge")]
    #[serde(skip)]
    bridge_poll_cck: u64,
    /// Tracks whose write has been handed to the drive but not yet laid on
    /// the platter. The library still holds the capture taken *before* such a
    /// write, so reading one of these would show the guest the disk as it was
    /// -- and, worse, could file that stale recording as proven. Nothing is
    /// read or cached for a track in here until its outcome arrives.
    #[cfg(feature = "fluxbridge")]
    #[serde(skip)]
    bridge_writing: Vec<usize>,
    /// The track a part-captured revolution is currently standing in for.
    /// Retains it between ticks: without a marker the next tick clears the
    /// cache and the guest sees nothing until the following poll.
    #[cfg(feature = "fluxbridge")]
    #[serde(skip)]
    bridge_partial_track: Option<usize>,
    /// Whether the interface going quiet has already been reported, so an
    /// unplugged drive says so once rather than once a frame.
    #[cfg(feature = "fluxbridge")]
    #[serde(skip)]
    bridge_reported_failed: bool,
    /// The track `cached` is holding filler for, while the interface captures
    /// it. `None` once the real revolution lands, so it is never mistaken for
    /// data and the cache is not rebuilt on every tick.
    #[cfg(feature = "fluxbridge")]
    #[serde(skip)]
    bridge_filler_track: Option<usize>,
    /// What is known about each track of the disk in a physical drive.
    ///
    /// Capturing a revolution costs a rotation of the platter -- about 200ms
    /// -- and the guest revisits tracks constantly: booting Workbench 1.3
    /// reads 63 distinct tracks 189 times, one of them fourteen times over.
    /// Keeping what has already been read turns every return trip into a
    /// memory copy. The whole disk is only a couple of megabytes, so nothing
    /// is evicted; what does empty it is the disk changing or a track being
    /// written, since those are the only ways what is on the platter stops
    /// matching. Only faithful recordings are kept, though -- see
    /// [`BridgeTrack`] for what qualifies and why.
    #[cfg(feature = "fluxbridge")]
    #[serde(skip)]
    bridge_tracks: Vec<BridgeTrack>,
    /// Which track [`Self::bridge_wait_since_cck`] is timing.
    #[cfg(feature = "fluxbridge")]
    #[serde(skip)]
    bridge_wait_track: Option<usize>,
    /// The revolution in hand has been turned all the way round. Only ever set
    /// for a physical drive serving a recording that cannot be trusted twice
    /// -- see [`Self::bridge_rev_seamless`] -- so the next one is fetched.
    #[cfg(feature = "fluxbridge")]
    #[serde(skip)]
    bridge_rev_spent: bool,
    /// Whether the revolution under the head closes on itself: index-aligned,
    /// verified clean, or restored from a kept recording. A revolution that
    /// does not is good for exactly one pass -- its ends were read a rotation
    /// apart, and any imperfection in how they were joined must not be shown
    /// to the guest twice.
    #[cfg(feature = "fluxbridge")]
    #[serde(skip)]
    bridge_rev_seamless: bool,
    /// Serving speed for kept captures in this bay: 100 (`normal`) or 200
    /// (`fast`). Compresses the cck-per-word fit when a captured revolution is
    /// replayed; the physical capture itself is untouched, so only replayed
    /// rotational waits shrink.
    #[cfg(feature = "fluxbridge")]
    #[serde(skip, default = "default_bridge_speed_percent")]
    bridge_speed_percent: u16,
    /// `elapsed_cck` when the drive was first asked for the track it is
    /// currently working on, and how many times it has been asked since. Only
    /// read to report how long a track took; 0 means "not waiting".
    #[cfg(feature = "fluxbridge")]
    #[serde(skip)]
    bridge_wait_since_cck: u64,
    #[cfg(feature = "fluxbridge")]
    #[serde(skip)]
    bridge_attempts: u32,
    cylinder: u8,
    motor_on: bool,
    motor_cck: u32,
    // Head position: bit `rotation_bit` of revolution `rotation_rev`, plus the
    // sub-cell time accumulator.
    rotation_rev: usize,
    rotation_bit: usize,
    rotation_acc_cck: u32,
    cached_track: Option<usize>,
    cached: CachedTrack,
    disk_change: bool,
    disk_change_sense: bool,
    write_protected_target: bool,
    write_protected_sense: bool,
    status_settle_cck: u32,
    // Head-move/settle countdown after a step: while non-zero the head is in
    // transit and read-data recovery is suppressed (the platter keeps spinning).
    // Position sense (/TRK0) and motor/RDY are unaffected. See SEEK_*_SETTLE_CCK.
    seek_settle_cck: u32,
    // Direction of the last step, to charge the longer reversal settle.
    last_step_inward: Option<bool>,
    // Timestamps (elapsed_cck) of the last accepted step pulse, and the last
    // accepted pulse in each direction, for the mechanism's pulse floor.
    last_step_cck: Option<u64>,
    last_step_inward_cck: Option<u64>,
    last_step_outward_cck: Option<u64>,
    external_id: u32,
    external_id_bit: u8,
    external_id_mode: bool,
    external_id_hold_deactivate: bool,
    // Cumulative spin time, for the COPPERLINE_DISK_SPEED_AFTER gate (lets the disk
    // run at full speed through boot, then be slowed for the demo).
    elapsed_cck: u64,
}

impl Default for FloppyDrive {
    fn default() -> Self {
        Self {
            #[cfg(feature = "fluxbridge")]
            bridge_wait_track: None,
            #[cfg(feature = "fluxbridge")]
            bridge_rev_spent: false,
            #[cfg(feature = "fluxbridge")]
            bridge_rev_seamless: true,
            #[cfg(feature = "fluxbridge")]
            bridge_speed_percent: 100,
            image: None,
            #[cfg(feature = "fluxbridge")]
            bridge: None,
            #[cfg(feature = "fluxbridge")]
            bridge_media: false,
            #[cfg(feature = "fluxbridge")]
            bridge_write_protected: true,
            #[cfg(feature = "fluxbridge")]
            bridge_tab_write_protected: true,
            #[cfg(feature = "fluxbridge")]
            bridge_poll_cck: 0,
            #[cfg(feature = "fluxbridge")]
            bridge_writing: Vec::new(),
            #[cfg(feature = "fluxbridge")]
            bridge_partial_track: None,
            #[cfg(feature = "fluxbridge")]
            bridge_reported_failed: false,
            #[cfg(feature = "fluxbridge")]
            bridge_filler_track: None,
            #[cfg(feature = "fluxbridge")]
            bridge_tracks: Vec::new(),
            #[cfg(feature = "fluxbridge")]
            bridge_wait_since_cck: 0,
            #[cfg(feature = "fluxbridge")]
            bridge_attempts: 0,
            cylinder: 0,
            motor_on: false,
            motor_cck: 0,
            rotation_rev: 0,
            rotation_bit: 0,
            rotation_acc_cck: 0,
            cached_track: None,
            cached: CachedTrack::default(),
            disk_change: false,
            disk_change_sense: false,
            write_protected_target: false,
            write_protected_sense: false,
            status_settle_cck: 0,
            seek_settle_cck: 0,
            last_step_inward: None,
            last_step_cck: None,
            last_step_inward_cck: None,
            last_step_outward_cck: None,
            external_id: STANDARD_EXTERNAL_DRIVE_ID,
            external_id_bit: 0,
            external_id_mode: false,
            external_id_hold_deactivate: false,
            elapsed_cck: 0,
        }
    }
}

impl FloppyDrive {
    fn load(config: &FloppyDriveConfig) -> Result<Self> {
        let image = FloppyImage::load(config)
            .with_context(|| format!("loading floppy image {}", config.path.display()))?;
        let write_protected = image.write_protected;
        Ok(Self {
            image: Some(image),
            disk_change: true,
            disk_change_sense: true,
            write_protected_target: write_protected,
            write_protected_sense: write_protected,
            ..Self::default()
        })
    }

    fn insert_image(&mut self, image: FloppyImage) {
        let write_protected = image.write_protected;
        self.image = Some(image);
        self.set_disk_change(true);
        self.set_write_protected(write_protected);
        self.cached_track = None;
        self.cached = CachedTrack::default();
        self.rotation_rev = 0;
        self.rotation_bit = 0;
        self.rotation_acc_cck = 0;
    }

    fn eject_image(&mut self) {
        self.image = None;
        self.set_disk_change(true);
        self.set_write_protected(false);
        self.cached_track = None;
        self.cached = CachedTrack::default();
    }

    /// Whether this drive is a real one on a bridge.
    fn is_bridged(&self) -> bool {
        #[cfg(feature = "fluxbridge")]
        {
            self.bridge.is_some()
        }
        #[cfg(not(feature = "fluxbridge"))]
        {
            false
        }
    }

    /// Whether there is anything under the head: a mounted image, or a real
    /// disk in a bridged drive. The drive's status lines key off this, so a
    /// bridge reports empty until a disk is actually inserted, exactly as an
    /// empty drive does.
    fn has_media(&self) -> bool {
        #[cfg(feature = "fluxbridge")]
        if self.bridge.is_some() {
            // Deliberately the cached answer, not a fresh query. This is read
            // from the drive's status lines constantly, and asking the device
            // each time lets a momentary false during a seek drop /RDY
            // mid-transfer, which the guest sees as the disk being yanked out
            // from under it. Refreshed by poll_bridge_media.
            return self.bridge_media;
        }
        self.image.is_some()
    }

    fn ready(&self) -> bool {
        self.has_media() && self.motor_on && self.motor_cck >= MOTOR_READY_CCK
    }

    fn rdy_line_asserted(&self) -> bool {
        if self.external_id_mode && !self.motor_on {
            return self.external_id_bit();
        }
        self.ready()
    }

    fn external_id_bit(&self) -> bool {
        if self.external_id_bit >= 32 {
            return false;
        }
        let shift = 31 - self.external_id_bit;
        self.external_id & (1 << shift) != 0
    }

    fn advance_external_id(&mut self) {
        if self.external_id_mode && !self.motor_on {
            if self.external_id_hold_deactivate {
                self.external_id_hold_deactivate = false;
                return;
            }
            self.external_id_bit = self.external_id_bit.saturating_add(1).min(32);
        }
    }

    fn latch_mtrxd(&mut self, motor_on: bool) {
        let was_on = self.motor_on;
        self.set_motor(motor_on);
        if motor_on {
            self.external_id_mode = false;
            self.external_id_bit = 0;
            self.external_id_hold_deactivate = false;
        } else if was_on {
            self.external_id_mode = true;
            self.external_id_bit = 0;
            self.external_id_hold_deactivate = true;
        }
    }

    fn reset_external_signal(&mut self) {
        self.set_motor(false);
        // A bus reset (DRESB) fully de-readies the drive rather than letting
        // the spin-up accumulator coast down; clear it explicitly.
        self.motor_cck = 0;
        self.external_id_mode = false;
        self.external_id_bit = 0;
        self.external_id_hold_deactivate = false;
        self.write_protected_sense = true;
        if self.image.is_none() && self.external_id != 0 {
            self.assert_no_media_change();
        }
        self.status_settle_cck = DISK_STATUS_SETTLE_CCK;
    }

    fn set_motor(&mut self, on: bool) {
        if self.motor_on == on {
            return;
        }
        // Follow the motor line on the real drive: it has to be spinning
        // before a track can be read, and parking it when the guest drops
        // the line keeps the physical drive from running continuously.
        #[cfg(feature = "fluxbridge")]
        if let Some(bridge) = self.bridge.as_mut() {
            // The library takes a surface here as well, which it would switch
            // to. Which one is passed does not matter: every read and write
            // names the side it wants, so the next track operation sets it
            // regardless, and the drive itself only has one motor.
            bridge.set_motor(false, on);
            // The cached track is deliberately kept across a motor stop. Every
            // fresh capture starts at a different point of the revolution, so
            // re-reading shifts the whole track under a guest that is part way
            // through it; the disk has not changed, so the copy in hand is
            // still what is on the platter.
        }
        self.motor_on = on;
        // Disk rotational inertia: a motor-off does not stop the platter
        // instantly. The spin-up accumulator is preserved here and decays
        // only while the motor stays off (see tick_motor). This matches real
        // drives, where /RDY survives the brief motor toggles some
        // trackloaders (e.g. Magic Pockets) issue between sector reads.
    }

    /// Applies a STEP pulse. Returns true when the pulse reached the
    /// stepper motor (the head moved, or banged the inner end stop),
    /// false when the mechanism swallowed it silently.
    fn step(&mut self, inward: bool) -> bool {
        // The change latch clears on the step PULSE itself (an electrical
        // edge), whether or not the mechanism accepts it.
        if self.has_media() {
            self.set_disk_change(false);
        }
        // The stepper ignores pulses spaced closer than the mechanism can
        // move (~40 us), and a reversal needs the same gap after the last
        // step the other way. A too-fast burst leaves the head where it is.
        let now = self.elapsed_cck;
        let since = |stamp: Option<u64>| stamp.map_or(u64::MAX, |t| now.saturating_sub(t));
        if crate::envcfg::flag("COPPERLINE_DIAG_STEP") {
            log::info!(
                "step pulse: dir={} delta={} cck",
                if inward { "in" } else { "out" },
                since(self.last_step_cck)
            );
        }
        if since(self.last_step_cck) < MIN_STEP_PULSE_CCK {
            return false;
        }
        let opposite = if inward {
            self.last_step_outward_cck
        } else {
            self.last_step_inward_cck
        };
        if since(opposite) < MIN_STEP_PULSE_CCK {
            return false;
        }
        self.last_step_cck = Some(now);
        if inward {
            self.last_step_inward_cck = Some(now);
        } else {
            self.last_step_outward_cck = Some(now);
        }
        let previous = self.cylinder;
        self.cylinder = if inward {
            self.cylinder.saturating_add(1).min((CYLINDERS - 1) as u8)
        } else {
            self.cylinder.saturating_sub(1)
        };
        if self.cylinder != previous {
            self.cached_track = None;
            // The head is now traversing: hold off read-data recovery for the
            // move time (longer when the head reverses direction). A burst of
            // steps keeps resetting this, so reads stay suppressed until the
            // settle elapses after the LAST step. /TRK0 and the cylinder index
            // above already updated, so seeking and recalibration stay instant.
            let reversal = self.last_step_inward.is_some_and(|prev| prev != inward);
            let settle = if reversal {
                SEEK_REVERSAL_SETTLE_CCK
            } else {
                SEEK_STEP_SETTLE_CCK
            };
            self.seek_settle_cck = self.seek_settle_cck.max(settle);
            debug!("floppy step: cylinder={}", self.cylinder);
        }
        self.last_step_inward = Some(inward);
        // 3.5" drives gate outward step pulses with the /TRK0 sensor:
        // at cylinder 0 the stepper never fires, so the head neither
        // moves nor makes a sound. There is no matching inner sensor,
        // so an inward pulse at the clamp still fires the stepper and
        // bangs the end stop.
        inward || previous != 0
    }

    /// True when the platter is stopped and no settle/seek countdown is
    /// pending, so `tick_motor` would only advance the diagnostic
    /// `elapsed_cck`. Used by the controller's idle fast-path.
    fn is_settled(&self) -> bool {
        !self.motor_on
            && self.motor_cck == 0
            && self.seek_settle_cck == 0
            && self.status_settle_cck == 0
    }

    fn tick_motor(&mut self, cck: u32) {
        self.elapsed_cck = self.elapsed_cck.saturating_add(cck as u64);
        self.seek_settle_cck = self.seek_settle_cck.saturating_sub(cck);
        if self.motor_on {
            self.motor_cck = self.motor_cck.saturating_add(cck).min(MOTOR_READY_CCK);
        } else {
            // Spin-down: the platter coasts to a stop over roughly the same
            // time it takes to reach speed. Brief motor-off pulses barely
            // dent the accumulator, so the drive stays ready across them.
            self.motor_cck = self.motor_cck.saturating_sub(cck);
        }
        let previous_status_settle = self.status_settle_cck;
        self.status_settle_cck = self.status_settle_cck.saturating_sub(cck);
        if previous_status_settle != 0 && self.status_settle_cck == 0 {
            self.disk_change_sense = self.disk_change;
            self.write_protected_sense = self.write_protected_target;
        }
    }

    fn set_disk_change(&mut self, changed: bool) {
        if self.disk_change != changed {
            self.disk_change = changed;
            self.status_settle_cck = DISK_STATUS_SETTLE_CCK;
        }
    }

    fn set_write_protected(&mut self, write_protected: bool) {
        if self.write_protected_target != write_protected {
            self.write_protected_target = write_protected;
            self.status_settle_cck = DISK_STATUS_SETTLE_CCK;
        }
    }

    fn assert_no_media_change(&mut self) {
        self.disk_change = true;
        self.disk_change_sense = true;
    }

    /// Advance disk rotation by `cck` cycles, returning whether an index
    /// pulse occurred and whether any word crossed matched `sync_word` (the
    /// free-running DSKSYNC comparator). `sync_word` is `None` when the
    /// comparator is disabled or this drive's stream is not feeding Paula.
    fn rev_count(&self) -> usize {
        self.cached.revs.len().max(1)
    }

    fn cur_rev(&self) -> Option<&TrackRev> {
        self.cached.rev(self.rotation_rev)
    }

    fn clamp_head(&mut self) {
        let revs = self.cached.revs.len();
        if revs == 0 {
            self.rotation_rev = 0;
            self.rotation_bit = 0;
            return;
        }
        self.rotation_rev %= revs;
        let bit_len = self.cached.revs[self.rotation_rev].bit_len.max(1);
        self.rotation_bit %= bit_len;
    }

    /// The MFM cell currently under the head.
    fn head_bit(&self) -> bool {
        self.cur_rev()
            .map(|r| r.bit(self.rotation_bit))
            .unwrap_or(false)
    }

    /// cck for the current cell.
    fn head_cell_cck(&self) -> u32 {
        let base = self
            .cur_rev()
            .map(|r| r.cell_cck(self.rotation_bit))
            .unwrap_or(1);
        // Diagnostic builds can slow disk rotation/read pacing by an integer
        // factor. Normal builds always use the modelled cell timing.
        if let Some((f, after)) = disk_speed_div() {
            if f > 1 {
                let elapsed_s = self.elapsed_cck as f64 / PAULA_CLOCK_HZ as f64;
                if elapsed_s >= after {
                    return base.saturating_mul(f).max(1);
                }
            }
        }
        base
    }

    /// cck for one 16-cell word at the current revolution (write pacing).
    fn head_word_cck(&self) -> u32 {
        self.cur_rev()
            .map(|r| r.word_cck)
            .unwrap_or_else(|| FloppyController::word_cck_for_track_words(encoded_track_words()))
    }

    /// cck remaining until the head will have advanced `bits` cells from its
    /// current position, accounting for the sub-cell time already accumulated
    /// and wrapping across whole revolutions as needed.
    fn head_cck_for_bits(&self, bits: usize) -> u64 {
        let Some(rev) = self.cur_rev() else {
            return bits as u64;
        };
        let bl = rev.bit_len;
        let start = self.rotation_bit % bl;
        let full_revs = (bits / bl) as u64;
        let rem = bits % bl;
        let end = start + rem;
        let span = if end <= bl {
            rev.prefix_cck(end) - rev.prefix_cck(start)
        } else {
            (rev.rev_cck() - rev.prefix_cck(start)) + rev.prefix_cck(end - bl)
        };
        let total = full_revs.saturating_mul(rev.rev_cck()).saturating_add(span);
        total.saturating_sub(self.rotation_acc_cck as u64)
    }

    /// Advance the head one cell. Returns true when it wraps past the index
    /// (end of the current revolution), cycling to the next captured
    /// revolution so weak/fuzzy bits vary per read.
    fn advance_head_bit(&mut self) -> bool {
        let Some(bit_len) = self.cur_rev().map(|r| r.bit_len) else {
            return false;
        };
        self.rotation_bit += 1;
        if self.rotation_bit >= bit_len {
            self.rotation_bit = 0;
            self.rotation_rev = (self.rotation_rev + 1) % self.rev_count();
            // A revolution that does not close on itself -- neither
            // index-aligned nor verified clean -- is good for exactly one
            // pass under the head. Mark it done; `ensure_track` replaces it
            // with the recording that followed rather than turning it again.
            #[cfg(feature = "fluxbridge")]
            if self.bridge.is_some() && !self.bridge_rev_seamless {
                self.bridge_rev_spent = true;
            }
            // A single-word (or shorter) track is too short to raise an index.
            bit_len > 16
        } else {
            false
        }
    }

    // Test accessors bridging the old word-grid view to the per-revolution
    // head: the current revolution's packed words, its word-aligned index
    // length, and a word-granular head position.
    #[cfg(test)]
    fn cached_words(&self) -> Vec<u16> {
        self.cached
            .revs
            .first()
            .map(|r| r.words.clone())
            .unwrap_or_default()
    }

    #[cfg(test)]
    fn cached_index_words(&self) -> usize {
        self.cached
            .revs
            .first()
            .map(|r| r.bit_len.div_ceil(16))
            .unwrap_or(0)
    }

    #[cfg(test)]
    fn set_rotation_word(&mut self, word: usize) {
        self.rotation_rev = 0;
        self.rotation_bit = word * 16;
        self.clamp_head();
    }

    #[cfg(test)]
    fn set_rotation_bit(&mut self, bit: usize) {
        self.rotation_rev = 0;
        self.rotation_bit = bit;
        self.clamp_head();
    }

    #[cfg(test)]
    fn rotation_word_index(&self) -> usize {
        self.rotation_bit / 16
    }
}

/// One captured/encoded revolution of a track as a packed MFM bit stream with
/// an exact bit length. The head reads bits and loops at `bit_len` (the index
/// boundary), so there is no word-rounding seam. `word_cck` is the cck for one
/// 16-bit word at this revolution's length; per-bit timing is derived so each
/// aligned 16-bit group sums to exactly `word_cck`, keeping synthetic (ADF)
/// word cadence identical to the old word-grid model.
///
/// `density` is empty for a uniform revolution. Otherwise it lists the runs
/// of cells written at a rate other than nominal -- the shape a mastering
/// protection's key track takes, where some sectors are written with cells a
/// few per cent longer or shorter than 2 us so a loader timing them can tell
/// the original from a copy -- and every cell's cck scales by its run's
/// per-mille weight.
#[derive(Clone, Default, serde::Serialize, serde::Deserialize)]
struct TrackRev {
    words: Vec<u16>,
    bit_len: usize,
    word_cck: u32,
    /// Every bit offset at which a 16-bit window ending there equals the
    /// sync word the index was built for, in ascending order. Derived from
    /// `words`, so it is built lazily on first use and neither serialized
    /// nor part of the emulated state; a save state rebuilds it on demand,
    /// and a DSKSYNC change replaces it.
    #[serde(skip)]
    sync_index: std::cell::RefCell<Option<SyncIndex>>,
    density: Vec<DensityRun>,
}

#[derive(Clone)]
struct SyncIndex {
    sync: u16,
    ends: Vec<u32>,
}

/// A run of cells from `start_bit` to the next run's start (or the index)
/// written at `permille` / 1000 of the nominal cell time. `weight_before` is
/// the per-mille weight summed over every cell ahead of the run, so a
/// cumulative cell time is one multiply from any bit inside it.
#[derive(Clone, Copy, serde::Serialize, serde::Deserialize)]
struct DensityRun {
    start_bit: usize,
    permille: u32,
    weight_before: u64,
}

impl TrackRev {
    /// A revolution of cells that carry no data, for a physical drive that has
    /// arrived at a track the interface has not finished capturing.
    ///
    /// Solid ones, which is what the hardware reads off unwritten media and
    /// what the FluxBridge library itself hands back before a buffer is
    /// ready. It cannot match a sync word, so nothing is recovered from it --
    /// the point is only that the platter keeps turning while the capture
    /// completes, so the guest's own rotational wait overlaps it rather than
    /// starting afterwards.
    #[cfg(feature = "fluxbridge")]
    fn filler(bit_len: usize, word_cck: u32) -> Self {
        Self {
            words: vec![0xFFFF; bit_len.div_ceil(16)],
            bit_len,
            word_cck: word_cck.max(1),
            sync_index: Default::default(),
            density: Vec::new(),
        }
    }

    fn new(words: Vec<u16>, bit_len: usize, word_cck: u32) -> Self {
        let bit_len = bit_len.min(words.len() * 16);
        Self {
            words,
            bit_len,
            word_cck: word_cck.max(1),
            sync_index: Default::default(),
            density: Vec::new(),
        }
    }

    /// A revolution whose cells are not all the nominal length: `spans`
    /// lists, in ascending bit order, where each new cell rate starts. A
    /// profile that is nominal throughout collapses to the uniform model.
    fn with_density(
        words: Vec<u16>,
        bit_len: usize,
        word_cck: u32,
        spans: &[formats::DensitySpan],
    ) -> Self {
        let mut rev = Self::new(words, bit_len, word_cck);
        if spans.iter().all(|span| span.permille == 1000) {
            return rev;
        }
        let mut runs: Vec<DensityRun> = Vec::with_capacity(spans.len() + 1);
        // The profile starts nominal unless a span says otherwise from bit 0.
        if spans.first().is_none_or(|span| span.start_bit != 0) {
            runs.push(DensityRun {
                start_bit: 0,
                permille: 1000,
                weight_before: 0,
            });
        }
        for span in spans {
            let start_bit = (span.start_bit as usize).min(rev.bit_len);
            if let Some(last) = runs.last() {
                if start_bit <= last.start_bit {
                    continue;
                }
            }
            runs.push(DensityRun {
                start_bit,
                permille: u32::from(span.permille).max(1),
                weight_before: 0,
            });
        }
        let mut weight = 0u64;
        for i in 0..runs.len() {
            runs[i].weight_before = weight;
            let end = runs.get(i + 1).map_or(rev.bit_len, |next| next.start_bit);
            weight += (end - runs[i].start_bit) as u64 * u64::from(runs[i].permille);
        }
        rev.density = runs;
        rev
    }

    fn bit(&self, bit: usize) -> bool {
        if self.bit_len == 0 {
            return false;
        }
        let bit = bit % self.bit_len;
        self.words[bit / 16] & (1 << (15 - (bit % 16))) != 0
    }

    /// The 16-bit MFM word starting at `bit` (MSB-first), wrapping at `bit_len`.
    fn word_at(&self, bit: usize) -> u16 {
        let mut value = 0u16;
        for offset in 0..16 {
            value = (value << 1) | u16::from(self.bit(bit + offset));
        }
        value
    }

    /// Cumulative cck from the start of the revolution to the start of `bit`.
    /// On a uniform revolution `prefix(16k) == k*word_cck` exactly, so aligned
    /// word boundaries match the old uniform word clock. With a density
    /// profile each cell is weighted by its run's per-mille rate instead, so a
    /// sector written with longer cells takes proportionally longer to pass
    /// the head -- which is what a protection's timing loop measures.
    fn prefix_cck(&self, bit: usize) -> u64 {
        if self.density.is_empty() {
            return (bit as u64 * self.word_cck as u64 + 8) / 16;
        }
        let run = self
            .density
            .iter()
            .rev()
            .find(|run| run.start_bit <= bit)
            .or(self.density.first())
            .expect("a density profile has at least one run");
        let weight =
            run.weight_before + bit.saturating_sub(run.start_bit) as u64 * u64::from(run.permille);
        (weight * self.word_cck as u64 + 8000) / 16000
    }

    fn cell_cck(&self, bit: usize) -> u32 {
        (self.prefix_cck(bit + 1) - self.prefix_cck(bit)).max(1) as u32
    }

    fn rev_cck(&self) -> u64 {
        self.prefix_cck(self.bit_len)
    }

    /// Bit distance from `from` to the next bit-aligned 16-bit window equal to
    /// `sync`, scanning forward within the revolution (wrapping once). Returns
    /// the number of bits until the matched window's last bit has been read.
    ///
    /// Answered from a per-sync-word index of matching window ends, so the
    /// cost is a binary search rather than a walk over the ~100k cells of a
    /// revolution: the STOP-state pacer asks this on every idle slice while
    /// a trackloader waits on DSKSYNC, and a linear scan there multiplies
    /// into seconds of host time per track (the Spooky Town loader ran at
    /// half real time through its loading phase).
    fn bits_until_sync(&self, from: usize, sync: u16) -> Option<usize> {
        if self.bit_len == 0 {
            return None;
        }
        let from = from % self.bit_len;
        let mut cache = self.sync_index.borrow_mut();
        if cache.as_ref().is_none_or(|index| index.sync != sync) {
            *cache = Some(self.build_sync_index(sync));
        }
        let index = cache.as_ref().expect("sync index was just built");
        if index.ends.is_empty() {
            return None;
        }
        let pos = index.ends.partition_point(|&end| (end as usize) < from);
        let end = match index.ends.get(pos) {
            Some(&end) => end as usize,
            None => index.ends[0] as usize + self.bit_len,
        };
        Some(end - from + 1)
    }

    /// Every bit offset `end` in the revolution at which the 16-bit window
    /// covering bits `end-15..=end` (wrapping at `bit_len`, as the head sees
    /// it) equals `sync`.
    fn build_sync_index(&self, sync: u16) -> SyncIndex {
        let mut ends = Vec::new();
        let mut window = 0u16;
        // Prime with the 15 cells that precede bit 0: the tail of the
        // revolution, so a window straddling the wrap is compared too.
        for back in (1..=15).rev() {
            let b = (self.bit_len - back % self.bit_len) % self.bit_len;
            window = (window << 1) | u16::from(self.bit(b));
        }
        for end in 0..self.bit_len {
            window = (window << 1) | u16::from(self.bit(end));
            if window == sync {
                ends.push(end as u32);
            }
        }
        SyncIndex { sync, ends }
    }
}

#[derive(Clone, Default, serde::Serialize, serde::Deserialize)]
struct CachedTrack {
    revs: Vec<TrackRev>,
}

/// What a physical drive's track is known to hold. Only a faithful recording
/// -- one whose two ends genuinely meet, so it can turn under the head like an
/// image's track -- is worth remembering: an index-aligned capture is faithful
/// by construction, and an index-less one only once it has been verified
/// clean. An index-less capture that cannot be verified is served exactly once
/// and marked, so a return visit asks the drive for the recording that
/// followed it rather than being shown the same one again.
#[cfg(feature = "fluxbridge")]
#[derive(Clone, Default)]
enum BridgeTrack {
    /// Never read since the disk went in.
    #[default]
    Unknown,
    /// Served once, but not a recording to be trusted twice.
    Unverified,
    /// A faithful recording, replayable indefinitely.
    Kept(CachedTrack),
}

impl CachedTrack {
    fn is_empty(&self) -> bool {
        self.revs.iter().all(|r| r.bit_len == 0)
    }

    fn rev(&self, idx: usize) -> Option<&TrackRev> {
        self.revs.get(idx).filter(|r| r.bit_len > 0)
    }
}

struct TrackStream {
    revs: Vec<TrackRev>,
}

fn apply_standard_adf_sectors(
    image_data: &mut [u8],
    track: usize,
    sectors: &[(usize, [u8; BYTES_PER_SECTOR])],
) {
    for (sector, sector_data) in sectors {
        let off = adf_sector_offset(track, *sector);
        image_data[off..off + BYTES_PER_SECTOR].copy_from_slice(sector_data);
    }
}

/// Write just the given sectors to their exact byte offsets in the ADF
/// file on disk, instead of rewriting the whole (up to several-hundred-KB)
/// image on every write-DMA completion: a disk write typically touches
/// only one or a handful of the image's sectors.
fn write_standard_adf_sectors_to_disk(
    path: &Path,
    track: usize,
    sectors: &[(usize, [u8; BYTES_PER_SECTOR])],
) -> Result<()> {
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false) // partial write: the rest of the file must survive
        .open(path)
        .with_context(|| format!("opening {} for write-through", path.display()))?;
    for (sector, sector_data) in sectors {
        let off = adf_sector_offset(track, *sector);
        file.seek(SeekFrom::Start(off as u64))
            .with_context(|| format!("seeking to sector {sector} in {}", path.display()))?;
        file.write_all(sector_data)
            .with_context(|| format!("writing sector {sector} to {}", path.display()))?;
    }
    Ok(())
}

fn apply_amigados_track_sectors(
    track_data: &mut [u8],
    track: usize,
    sectors: &[(usize, [u8; BYTES_PER_SECTOR])],
) -> Result<()> {
    let sectors_per_track = track_data.len() / BYTES_PER_SECTOR;
    ensure!(
        sectors_per_track > 0,
        "target AmigaDOS track {track} has no sector payload"
    );
    for (sector, sector_data) in sectors {
        ensure!(
            *sector < sectors_per_track,
            "decoded sector {} is outside target track {} sector count {}",
            *sector,
            track,
            sectors_per_track
        );
        let off = *sector * BYTES_PER_SECTOR;
        track_data[off..off + BYTES_PER_SECTOR].copy_from_slice(sector_data);
    }
    Ok(())
}

fn apply_extended_adf_write(
    tracks: &mut [Option<FloppyTrackImage>],
    track: usize,
    write_words: &[u16],
    write_start_word: usize,
    write_start_bit: u8,
    legacy_extended_adf: bool,
    lose_tail_bits: bool,
) -> Result<()> {
    let Some(Some(image_track)) = tracks.get_mut(track) else {
        bail!("target track {track} is empty");
    };
    match image_track {
        FloppyTrackImage::AmigaDos(track_data) => {
            let sectors = decode_non_empty_track_write(track, write_words)?;
            apply_amigados_track_sectors(track_data, track, &sectors)?;
        }
        FloppyTrackImage::RawMfm {
            words,
            bit_len,
            stored_len,
            revolutions,
            legacy_sync,
            bitcell_ns,
            density,
        } => {
            // A track the guest wrote over carries the drive's own uniform
            // cell rate, not the mastered profile.
            *bitcell_ns = None;
            *density = None;
            if legacy_extended_adf || legacy_sync.is_some() {
                apply_legacy_raw_mfm_write(
                    words,
                    bit_len,
                    stored_len,
                    revolutions,
                    legacy_sync,
                    write_words,
                    write_start_word,
                    write_start_bit,
                    lose_tail_bits,
                )?;
            } else {
                apply_raw_mfm_write(
                    words,
                    bit_len,
                    stored_len,
                    revolutions,
                    write_words,
                    write_start_word,
                    write_start_bit,
                    lose_tail_bits,
                )?;
            }
        }
    }
    Ok(())
}

fn decode_non_empty_track_write(
    track: usize,
    write_words: &[u16],
) -> Result<Vec<(usize, [u8; BYTES_PER_SECTOR])>> {
    let sectors = decode_track_write(track, write_words)?;
    ensure!(!sectors.is_empty(), "no valid AmigaDOS sectors");
    Ok(sectors)
}

fn apply_raw_mfm_write(
    words: &mut Vec<u16>,
    bit_len: &mut u32,
    stored_len: &mut usize,
    revolutions: &mut u8,
    write_words: &[u16],
    write_start_word: usize,
    write_start_bit: u8,
    lose_tail_bits: bool,
) -> Result<()> {
    ensure!(!write_words.is_empty(), "raw write stream is empty");
    ensure!(
        write_words.len() <= (u32::MAX as usize / 16),
        "raw write stream is too long"
    );
    let Some(geometry) = raw_mfm_bit_geometry(words.len(), *bit_len, *stored_len, *revolutions)
    else {
        replace_raw_mfm_write(
            words,
            bit_len,
            stored_len,
            revolutions,
            write_words,
            lose_tail_bits,
        );
        return Ok(());
    };
    overlay_raw_mfm_bits(
        words,
        geometry,
        write_start_word,
        write_start_bit,
        write_words,
        lose_tail_bits,
    );
    Ok(())
}

fn apply_legacy_raw_mfm_write(
    words: &mut Vec<u16>,
    bit_len: &mut u32,
    stored_len: &mut usize,
    revolutions: &mut u8,
    legacy_sync: &mut Option<u16>,
    write_words: &[u16],
    write_start_word: usize,
    write_start_bit: u8,
    lose_tail_bits: bool,
) -> Result<()> {
    ensure!(!write_words.is_empty(), "legacy raw write stream is empty");
    ensure!(
        write_words.len() <= (u16::MAX as usize / 2) + 1,
        "legacy raw write stream is too long"
    );
    ensure!(
        write_words.len() <= (u32::MAX as usize / 16),
        "legacy raw write stream is too long"
    );
    if words.is_empty() {
        ensure!(
            write_words.len() >= 2,
            "legacy raw write stream needs sync plus payload"
        );
        words.extend_from_slice(write_words);
        if lose_tail_bits {
            clear_lost_disk_write_bits(words, disk_write_effective_bits(write_words));
        }
        *bit_len = (write_words.len() as u32) * 16;
        *stored_len = (write_words.len() - 1) * 2;
        *revolutions = 1;
        *legacy_sync = write_words.first().copied();
        return Ok(());
    }
    let geometry = legacy_raw_mfm_bit_geometry(words.len(), *bit_len);
    overlay_raw_mfm_bits(
        words,
        geometry,
        write_start_word,
        write_start_bit,
        write_words,
        lose_tail_bits,
    );
    *bit_len = geometry.valid_bits_per_rev as u32;
    *stored_len = geometry.valid_bits_per_rev.saturating_sub(16).div_ceil(8);
    *revolutions = 1;
    *legacy_sync = words.first().copied();
    Ok(())
}

fn replace_raw_mfm_write(
    words: &mut Vec<u16>,
    bit_len: &mut u32,
    stored_len: &mut usize,
    revolutions: &mut u8,
    write_words: &[u16],
    lose_tail_bits: bool,
) {
    let write_len = write_words.len().saturating_mul(2);
    let capacity_bytes = (*stored_len).max(write_len);
    let capacity_words = capacity_bytes.div_ceil(2).max(write_words.len());
    words.clear();
    words.extend_from_slice(write_words);
    if lose_tail_bits {
        clear_lost_disk_write_bits(words, disk_write_effective_bits(write_words));
    }
    words.resize(capacity_words, 0);
    *bit_len = (write_words.len() as u32) * 16;
    *stored_len = capacity_bytes;
    *revolutions = 1;
}

#[derive(Clone, Copy)]
struct RawMfmBitGeometry {
    valid_bits_per_rev: usize,
    words_per_rev: usize,
    revolutions: usize,
}

impl RawMfmBitGeometry {
    fn full_words(words: usize) -> Self {
        Self {
            valid_bits_per_rev: words.saturating_mul(16).max(1),
            words_per_rev: words.max(1),
            revolutions: 1,
        }
    }

    fn total_bits(self) -> usize {
        self.valid_bits_per_rev
            .saturating_mul(self.revolutions)
            .max(1)
    }

    fn stream_words(self) -> usize {
        self.words_per_rev.saturating_mul(self.revolutions).max(1)
    }

    fn logical_bit_from_storage(self, word: usize, bit: u8) -> usize {
        let stream_word = word % self.stream_words();
        let rev = stream_word / self.words_per_rev;
        let word_in_rev = stream_word % self.words_per_rev;
        let bit_in_rev = word_in_rev.saturating_mul(16) + usize::from(bit.min(15));
        (rev.saturating_mul(self.valid_bits_per_rev) + bit_in_rev) % self.total_bits()
    }

    fn storage_from_logical_bit(self, bit: usize) -> (usize, usize) {
        let bit = bit % self.total_bits();
        let rev = bit / self.valid_bits_per_rev;
        let bit_in_rev = bit % self.valid_bits_per_rev;
        (
            rev.saturating_mul(self.words_per_rev) + bit_in_rev / 16,
            15 - (bit_in_rev % 16),
        )
    }
}

fn raw_mfm_bit_geometry(
    words_len: usize,
    bit_len: u32,
    stored_len: usize,
    revolutions: u8,
) -> Option<RawMfmBitGeometry> {
    if words_len == 0 || bit_len == 0 {
        return None;
    }
    let valid_bits_per_rev = bit_len as usize;
    let words_per_rev = valid_bits_per_rev.div_ceil(16).max(1);
    let stored_words = stored_len.div_ceil(2).min(words_len);
    let revolutions = (revolutions.max(1) as usize).min(stored_words / words_per_rev);
    (revolutions > 0).then_some(RawMfmBitGeometry {
        valid_bits_per_rev,
        words_per_rev,
        revolutions,
    })
}

fn legacy_raw_mfm_bit_geometry(words_len: usize, bit_len: u32) -> RawMfmBitGeometry {
    if words_len == 0 || bit_len == 0 {
        return RawMfmBitGeometry::full_words(words_len);
    }
    let valid_bits_per_rev = (bit_len as usize).min(words_len.saturating_mul(16)).max(1);
    RawMfmBitGeometry {
        valid_bits_per_rev,
        words_per_rev: valid_bits_per_rev.div_ceil(16).min(words_len).max(1),
        revolutions: 1,
    }
}

fn overlay_raw_mfm_bits(
    words: &mut [u16],
    geometry: RawMfmBitGeometry,
    write_start_word: usize,
    write_start_bit: u8,
    write_words: &[u16],
    lose_tail_bits: bool,
) {
    let stream_words = geometry.stream_words().min(words.len()).max(1);
    let start_bit = geometry.logical_bit_from_storage(write_start_word, write_start_bit);
    let write_bits = if lose_tail_bits {
        disk_write_effective_bits(write_words)
    } else {
        write_words.len().saturating_mul(16)
    };
    for bit_idx in 0..write_bits {
        let src_word = write_words[bit_idx / 16];
        let src_bit = 15 - (bit_idx % 16);
        let bit = (src_word >> src_bit) & 1;

        let logical_bit = start_bit + bit_idx;
        let (dst_word, dst_bit) = geometry.storage_from_logical_bit(logical_bit);
        let dst_word = dst_word % stream_words;
        let mask = 1u16 << dst_bit;
        if bit != 0 {
            words[dst_word] |= mask;
        } else {
            words[dst_word] &= !mask;
        }
    }
}

fn disk_write_effective_bits(write_words: &[u16]) -> usize {
    write_words
        .len()
        .saturating_mul(16)
        .saturating_sub(DISK_WRITE_LOST_BITS)
}

fn clear_lost_disk_write_bits(words: &mut [u16], effective_bits: usize) {
    let total_bits = words.len().saturating_mul(16);
    for bit_idx in effective_bits.min(total_bits)..total_bits {
        let word = bit_idx / 16;
        let bit = 15 - (bit_idx % 16);
        words[word] &= !(1 << bit);
    }
}

fn encode_uae_extended_adf(tracks: &[Option<FloppyTrackImage>]) -> Result<Vec<u8>> {
    ensure!(
        tracks.len() <= u16::MAX as usize,
        "too many tracks for UAE-1ADF image"
    );
    let mut descriptors = Vec::with_capacity(tracks.len() * 12);
    let mut payloads = Vec::new();
    for track in tracks {
        match track {
            None => {
                descriptors.extend_from_slice(&[0; 12]);
            }
            Some(FloppyTrackImage::AmigaDos(data)) => {
                descriptors.extend_from_slice(&0u16.to_be_bytes());
                descriptors.push(0);
                descriptors.push(0);
                descriptors.extend_from_slice(&(data.len() as u32).to_be_bytes());
                descriptors.extend_from_slice(&((data.len() * 8) as u32).to_be_bytes());
                payloads.extend_from_slice(data);
            }
            Some(FloppyTrackImage::RawMfm {
                words,
                bit_len,
                stored_len,
                revolutions,
                ..
            }) => {
                let payload = raw_words_payload(
                    words,
                    stored_len.saturating_mul(8).min(u32::MAX as usize) as u32,
                    0,
                );
                ensure!(
                    payload.len() == *stored_len,
                    "UAE-1ADF raw track payload is shorter than stored length"
                );
                descriptors.extend_from_slice(&0u16.to_be_bytes());
                descriptors.push(revolutions.saturating_sub(1));
                descriptors.push(1);
                descriptors.extend_from_slice(&(payload.len() as u32).to_be_bytes());
                descriptors.extend_from_slice(&bit_len.to_be_bytes());
                payloads.extend_from_slice(&payload);
            }
        }
    }

    let mut image = Vec::with_capacity(12 + descriptors.len() + payloads.len());
    image.extend_from_slice(UAE_EXT2_SIGNATURE);
    image.extend_from_slice(&0u16.to_be_bytes());
    image.extend_from_slice(&(tracks.len() as u16).to_be_bytes());
    image.extend_from_slice(&descriptors);
    image.extend_from_slice(&payloads);
    Ok(image)
}

fn encode_uae_legacy_extended_adf(tracks: &[Option<FloppyTrackImage>]) -> Result<Vec<u8>> {
    ensure!(
        tracks.len() <= 160,
        "too many tracks for legacy UAE--ADF image"
    );
    let mut descriptors = Vec::with_capacity(160 * 4);
    let mut payloads = Vec::new();
    for idx in 0..160 {
        match tracks.get(idx).and_then(|track| track.as_ref()) {
            None => {
                descriptors.extend_from_slice(&0u16.to_be_bytes());
                descriptors.extend_from_slice(&0u16.to_be_bytes());
            }
            Some(FloppyTrackImage::AmigaDos(data)) => {
                ensure!(
                    data.len() <= u16::MAX as usize,
                    "legacy UAE--ADF AmigaDOS track {idx} is too large"
                );
                descriptors.extend_from_slice(&0u16.to_be_bytes());
                descriptors.extend_from_slice(&(data.len() as u16).to_be_bytes());
                payloads.extend_from_slice(data);
            }
            Some(FloppyTrackImage::RawMfm {
                words,
                bit_len,
                legacy_sync,
                ..
            }) => {
                let sync = legacy_sync
                    .or_else(|| words.first().copied())
                    .unwrap_or(DEFAULT_DSKSYNC);
                let skip_words = usize::from(words.first().copied() == Some(sync));
                let payload = raw_words_payload(words, *bit_len, skip_words);
                ensure!(
                    payload.len() <= u16::MAX as usize,
                    "legacy UAE--ADF raw track {idx} is too large"
                );
                descriptors.extend_from_slice(&sync.to_be_bytes());
                descriptors.extend_from_slice(&(payload.len() as u16).to_be_bytes());
                payloads.extend_from_slice(&payload);
            }
        }
    }

    let mut image = Vec::with_capacity(8 + descriptors.len() + payloads.len());
    image.extend_from_slice(UAE_EXT1_SIGNATURE);
    image.extend_from_slice(&descriptors);
    image.extend_from_slice(&payloads);
    Ok(image)
}

fn raw_words_payload(words: &[u16], bit_len: u32, skip_words: usize) -> Vec<u8> {
    let byte_count = (bit_len as usize).div_ceil(8);
    let skip_bytes = skip_words.saturating_mul(2);
    let keep_bytes = byte_count.saturating_sub(skip_bytes);
    let mut payload: Vec<u8> = words
        .iter()
        .copied()
        .skip(skip_words)
        .flat_map(u16::to_be_bytes)
        .collect();
    payload.truncate(keep_bytes);
    payload
}

#[derive(serde::Serialize, serde::Deserialize)]
struct DiskDma {
    drive: usize,
    track: usize,
    write: bool,
    remaining: u32,
    wait_sync: bool,
    msb_sync: bool,
    write_words: Vec<u16>,
    write_start_word: usize,
    write_start_bit: u8,
    /// Set when a write arms without a delivering mechanism. The rotational
    /// start is re-latched from the first word actually consumed.
    write_start_pending: bool,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct DiskDirectWrite {
    drive: usize,
    track: usize,
    write_words: Vec<u16>,
    write_start_word: usize,
    write_start_bit: u8,
}

// Bit-granular accessor over a packed MFM word slice. The live read path uses
// `TrackRev::word_at`/`byte_at` directly; this remains for the bit-stream and
// DPLL-FIFO unit tests.
#[cfg(test)]
struct DiskBitStream<'a> {
    words: &'a [u16],
    index_words: usize,
    bit_pos: usize,
}

#[cfg(test)]
impl<'a> DiskBitStream<'a> {
    fn from_word_phase(
        words: &'a [u16],
        index_words: usize,
        word: usize,
        bit_phase: u8,
    ) -> Option<Self> {
        if words.is_empty() {
            return None;
        }
        let stream_words = words.len();
        Some(Self {
            words,
            index_words: index_words.max(1).min(stream_words),
            bit_pos: (word % stream_words) * 16 + usize::from(bit_phase.min(15)),
        })
    }

    #[cfg_attr(not(test), allow(dead_code))]
    fn from_rotation(
        words: &'a [u16],
        index_words: usize,
        rotation_word: usize,
        rotation_acc_cck: u32,
        word_cck: u32,
    ) -> Option<Self> {
        Self::from_word_phase(
            words,
            index_words,
            rotation_word,
            disk_bit_phase(rotation_acc_cck, word_cck),
        )
    }

    #[cfg_attr(not(test), allow(dead_code))]
    fn bit_position(&self) -> usize {
        self.bit_pos % self.stream_bits()
    }

    #[cfg_attr(not(test), allow(dead_code))]
    fn index_position(&self) -> usize {
        self.bit_pos % self.index_bits()
    }

    fn storage_word_position(&self) -> usize {
        self.bit_position() / 16
    }

    fn storage_word(&self) -> u16 {
        self.words[self.storage_word_position()]
    }

    fn assembled_byte(&self) -> u8 {
        self.assemble_bits(8) as u8
    }

    #[cfg_attr(not(test), allow(dead_code))]
    fn assembled_word(&self) -> u16 {
        self.assemble_bits(16)
    }

    #[cfg_attr(not(test), allow(dead_code))]
    fn sync_matches(&self, sync: u16) -> bool {
        self.assembled_word() == sync
    }

    fn assemble_bits(&self, bits: usize) -> u16 {
        debug_assert!(bits <= 16);
        let mut value = 0u16;
        for offset in 0..bits.min(16) {
            value <<= 1;
            value |= u16::from(self.bit_at_offset(offset));
        }
        value
    }

    fn bit_at_offset(&self, offset: usize) -> bool {
        let bit_pos = (self.bit_pos + offset) % self.stream_bits();
        let word = self.words[bit_pos / 16];
        let bit = 15 - (bit_pos % 16);
        word & (1 << bit) != 0
    }

    fn stream_bits(&self) -> usize {
        self.words.len() * 16
    }

    fn index_bits(&self) -> usize {
        self.index_words * 16
    }
}

// Paula's disk read shifter + read FIFO. Fed one recovered MFM cell at a time
// as the selected drive's head rotates: it shifts bits MSB-first, raises
// DSKBYT each assembled byte, compares the running 16-bit window to DSKSYNC
// every bit (so sync is detected bit-aligned, not on a fixed word grid), and
// frames read-DMA words into a 3-word FIFO. Word framing realigns to the sync
// bit phase so the post-sync word stream matches the disk's framing, and with
// WORDSYNC set it does so again at every later match.
#[derive(serde::Serialize, serde::Deserialize)]
struct PaulaDiskReadDpllFifo {
    shift_word: u16,
    bit_offset: u8,
    fifo: [u16; 3],
    fifo_len: usize,
    dskbytr_byte: u8,
    dskbyt: bool,
    word_equal: bool,
    sync_irq: bool,
    fifo_overflow: bool,
}

impl PaulaDiskReadDpllFifo {
    fn new() -> Self {
        Self {
            shift_word: 0,
            bit_offset: 0,
            fifo: [0; 3],
            fifo_len: 0,
            dskbytr_byte: 0,
            dskbyt: false,
            word_equal: false,
            sync_irq: false,
            fifo_overflow: false,
        }
    }

    /// Reset queued DMA words for a fresh transfer while preserving Paula's
    /// recovered 16-bit disk word phase. The bit shifter itself keeps running
    /// on the live stream; DMA start must not re-phase it to the CPU write.
    fn reset_framing_to_phase(&mut self, bit_phase: u8) {
        self.bit_offset = bit_phase & 15;
        self.fifo_len = 0;
        self.fifo_overflow = false;
    }

    /// Realign the word framing so the next 16 sampled bits form the next FIFO
    /// word. Called when a sync-wait read locks onto its sync mark.
    fn realign(&mut self) {
        self.bit_offset = 0;
        self.fifo_len = 0;
        self.fifo_overflow = false;
    }

    /// Restart word framing at the cell after a sync match made mid-transfer,
    /// keeping the words already framed and queued for DMA. The partial word
    /// the sync interrupted is dropped, as on the hardware.
    fn reframe(&mut self) {
        self.bit_offset = 0;
    }

    /// Whether the cell just sampled completed a DSKSYNC match: the
    /// comparator's level, where `take_sync_irq` reports only its edge.
    fn sync_matched(&self) -> bool {
        self.word_equal
    }

    /// Re-evaluate the comparator against a freshly written DSKSYNC: it is
    /// combinational on the shift register, so the level follows the new
    /// value at once. The edge that raises DSKSYN belongs to the cell that
    /// completes a match, so it is left to `sample_bit`.
    fn set_sync(&mut self, sync: u16) {
        self.word_equal = self.shift_word == sync;
    }

    /// The byte the shifter last completed, if one has completed since the
    /// previous call: DSKBYT, cleared by the DSKBYTR read that consumes it.
    fn take_dskbyt(&mut self) -> Option<u8> {
        if !self.dskbyt {
            return None;
        }
        self.dskbyt = false;
        Some(self.dskbytr_byte)
    }

    #[cfg(test)]
    fn sample_stream_bits(&mut self, stream: &DiskBitStream<'_>, bits: usize, sync: u16) {
        self.sample_stream_range(stream, 0, bits, sync);
    }

    #[cfg(test)]
    fn sample_stream_range(
        &mut self,
        stream: &DiskBitStream<'_>,
        start_bit: usize,
        bits: usize,
        sync: u16,
    ) {
        for bit in start_bit..start_bit + bits {
            self.sample_bit(stream.bit_at_offset(bit), sync, true);
        }
    }

    /// Shift in one cell. `store` enables pushing completed words into the read
    /// FIFO (set while a read DMA is transferring; clear while waiting for sync
    /// or free-running).
    fn sample_bit(&mut self, bit: bool, sync: u16, store: bool) {
        self.shift_word = (self.shift_word << 1) | u16::from(bit);

        if self.bit_offset == 7 || self.bit_offset == 15 {
            self.dskbytr_byte = (self.shift_word & 0x00FF) as u8;
            self.dskbyt = true;
        }

        if self.shift_word == sync {
            if !self.word_equal {
                self.sync_irq = true;
            }
            self.word_equal = true;
        } else {
            self.word_equal = false;
        }

        if self.bit_offset == 15 && store {
            self.push_fifo_word(self.shift_word);
        }

        self.bit_offset = (self.bit_offset + 1) & 15;
    }

    #[cfg(test)]
    fn read_dskbytr(&mut self) -> u16 {
        let mut status = u16::from(self.dskbytr_byte);
        if self.dskbyt {
            status |= DSKBYT;
            self.dskbyt = false;
        }
        if self.word_equal {
            status |= WORDEQUAL;
        }
        status
    }

    /// Cells already shifted toward the next framed word (0..15).
    fn framing_bits(&self) -> usize {
        self.bit_offset as usize
    }

    fn read_fifo_word(&mut self) -> Option<u16> {
        if self.fifo_len == 0 {
            return None;
        }
        let word = self.fifo[0];
        for idx in 1..self.fifo_len {
            self.fifo[idx - 1] = self.fifo[idx];
        }
        self.fifo_len -= 1;
        Some(word)
    }

    #[cfg(test)]
    fn fifo_len(&self) -> usize {
        self.fifo_len
    }

    #[cfg(test)]
    fn fifo_overflowed(&self) -> bool {
        self.fifo_overflow
    }

    fn take_sync_irq(&mut self) -> bool {
        std::mem::take(&mut self.sync_irq)
    }

    fn push_fifo_word(&mut self, word: u16) {
        if self.fifo_len == self.fifo.len() {
            self.fifo_overflow = true;
            return;
        }
        self.fifo[self.fifo_len] = word;
        self.fifo_len += 1;
    }
}

#[cfg(test)]
fn disk_bit_phase(rotation_acc_cck: u32, word_cck: u32) -> u8 {
    if word_cck == 0 {
        return 0;
    }
    ((u64::from(rotation_acc_cck) * 16) / u64::from(word_cck)).min(15) as u8
}

fn encoded_track_words() -> usize {
    TRACK_GAP_LONGS * 2 + SECTORS_PER_TRACK * AMIGADOS_SECTOR_MFM_WORDS + TRACK_TRAILER_WORDS
}

fn encode_adf_track(track: usize, adf: &[u8]) -> Vec<u16> {
    let off = adf_sector_offset(track, 0);
    encode_amigados_track(track, &adf[off..off + SECTORS_PER_TRACK * BYTES_PER_SECTOR])
}

fn encode_amigados_track(track: usize, track_data: &[u8]) -> Vec<u16> {
    let sectors_per_track = track_data.len() / BYTES_PER_SECTOR;
    let mut longs = vec![0xAAAA_AAAAu32; TRACK_GAP_LONGS];
    for sector in 0..sectors_per_track {
        push_sector(track, sector, sectors_per_track, track_data, &mut longs);
    }
    let mut words = Vec::with_capacity(encoded_track_words());
    for long in longs {
        words.push((long >> 16) as u16);
        words.push(long as u16);
    }
    push_track_trailer(&mut words);
    words
}

fn push_track_trailer(words: &mut Vec<u16>) {
    for trailer_idx in 0..TRACK_TRAILER_WORDS {
        let last_bit_set = words.last().copied().unwrap_or(0) & 1 != 0;
        let word = if trailer_idx + 1 == TRACK_TRAILER_WORDS {
            if last_bit_set {
                0x2AA8
            } else {
                0xAAA8
            }
        } else if last_bit_set {
            0x2AAA
        } else {
            0xAAAA
        };
        words.push(word);
    }
}

fn push_sector(
    track: usize,
    sector: usize,
    sectors_per_track: usize,
    track_data: &[u8],
    longs: &mut Vec<u32>,
) {
    let gap = if longs.last().copied().unwrap_or(0) & 1 != 0 {
        0x2AAA_AAAA
    } else {
        0xAAAA_AAAA
    };
    longs.push(gap);
    longs.push(0x4489_4489);

    let mut header = [0u8; 20];
    header[0] = 0xFF;
    header[1] = track as u8;
    header[2] = sector as u8;
    header[3] = (sectors_per_track - sector) as u8;

    let data_off = sector * BYTES_PER_SECTOR;
    let data = &track_data[data_off..data_off + BYTES_PER_SECTOR];
    let header_checksum = checksum_decoded_bytes(&header);
    let data_checksum = checksum_decoded_bytes(data);

    encode_block(&header[0..4], longs);
    encode_block(&header[4..20], longs);
    encode_block(&header_checksum.to_be_bytes(), longs);
    encode_block(&data_checksum.to_be_bytes(), longs);
    encode_block(data, longs);
}

fn encode_block(src: &[u8], dest: &mut Vec<u32>) {
    debug_assert_eq!(src.len() % 4, 0);
    let longs: Vec<u32> = src
        .chunks_exact(4)
        .map(|c| u32::from_be_bytes([c[0], c[1], c[2], c[3]]))
        .collect();
    for &long in &longs {
        push_encoded_bits(long >> 1, dest);
    }
    for &long in &longs {
        push_encoded_bits(long, dest);
    }
}

fn push_encoded_bits(data: u32, dest: &mut Vec<u32>) {
    let mut encoded = data & MFM_MASK;
    let inv = encoded ^ MFM_MASK;
    encoded |= ((inv >> 1) | 0x8000_0000) & (inv << 1);
    if dest.last().copied().unwrap_or(0) & 1 != 0 {
        encoded &= 0x7FFF_FFFF;
    }
    dest.push(encoded);
}

fn decode_track_write(track: usize, words: &[u16]) -> Result<Vec<(usize, [u8; BYTES_PER_SECTOR])>> {
    let mut sectors = Vec::new();
    let mut pos = 0usize;
    while pos + 2 < words.len() {
        if words[pos] != DEFAULT_DSKSYNC {
            pos += 1;
            continue;
        }
        while pos < words.len() && words[pos] == DEFAULT_DSKSYNC {
            pos += 1;
        }

        let Some((info, p)) = decode_block(words, pos, 4) else {
            break;
        };
        pos = p;
        let Some((label, p)) = decode_block(words, pos, 16) else {
            break;
        };
        pos = p;
        let Some((hdrchk, p)) = decode_block(words, pos, 4) else {
            break;
        };
        pos = p;
        let Some((datachk, p)) = decode_block(words, pos, 4) else {
            break;
        };
        pos = p;
        let Some((data, p)) = decode_block(words, pos, BYTES_PER_SECTOR) else {
            break;
        };
        pos = p;

        let mut header = Vec::with_capacity(20);
        header.extend_from_slice(&info);
        header.extend_from_slice(&label);
        let stored_header_checksum = u32::from_be_bytes(hdrchk.try_into().unwrap());
        let stored_data_checksum = u32::from_be_bytes(datachk.try_into().unwrap());
        if checksum_decoded_bytes(&header) != stored_header_checksum {
            continue;
        }
        if checksum_decoded_bytes(&data) != stored_data_checksum {
            continue;
        }
        if info[0] != 0xFF || info[1] as usize != track || info[2] as usize >= SECTORS_PER_TRACK {
            continue;
        }

        let mut sector_data = [0u8; BYTES_PER_SECTOR];
        sector_data.copy_from_slice(&data);
        sectors.push((info[2] as usize, sector_data));
    }
    Ok(sectors)
}

fn decode_block(words: &[u16], pos: usize, bytes_len: usize) -> Option<(Vec<u8>, usize)> {
    if !bytes_len.is_multiple_of(4) {
        return None;
    }
    let long_count = bytes_len / 4;
    let encoded_longs = long_count * 2;
    if pos + encoded_longs * 2 > words.len() {
        return None;
    }

    let mut encoded = Vec::with_capacity(encoded_longs);
    for i in 0..encoded_longs {
        let hi = words[pos + i * 2] as u32;
        let lo = words[pos + i * 2 + 1] as u32;
        encoded.push((hi << 16) | lo);
    }

    let mut out = Vec::with_capacity(bytes_len);
    for i in 0..long_count {
        let odd = encoded[i];
        let even = encoded[i + long_count];
        let decoded = (even & MFM_MASK) | ((odd & MFM_MASK) << 1);
        out.extend_from_slice(&decoded.to_be_bytes());
    }
    Some((out, pos + encoded_longs * 2))
}

fn checksum_decoded_bytes(data: &[u8]) -> u32 {
    debug_assert_eq!(data.len() % 4, 0);
    let mut checksum = 0u32;
    for chunk in data.chunks_exact(4) {
        let long = u32::from_be_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
        checksum ^= (long >> 1) & MFM_MASK;
        checksum ^= long & MFM_MASK;
    }
    checksum & MFM_MASK
}

fn adf_sector_offset(track: usize, sector: usize) -> usize {
    (track * SECTORS_PER_TRACK + sector) * BYTES_PER_SECTOR
}

fn read_chip_word(chip_ram: &[u8], addr: u32) -> u16 {
    if chip_ram.is_empty() {
        return 0;
    }
    let off = (addr as usize) % chip_ram.len();
    let hi = chip_ram[off];
    let lo = chip_ram[(off + 1) % chip_ram.len()];
    u16::from_be_bytes([hi, lo])
}

fn write_chip_word(chip_ram: &mut [u8], addr: u32, word: u16) {
    if chip_ram.is_empty() {
        return;
    }
    let off = (addr as usize) % chip_ram.len();
    let [hi, lo] = word.to_be_bytes();
    chip_ram[off] = hi;
    chip_ram[(off + 1) % chip_ram.len()] = lo;
}

mod formats;
mod transfer;

#[cfg(test)]
mod tests;
