// SPDX-License-Identifier: GPL-3.0-or-later

//! Standard Amiga DD floppy/ADF support.
//!
//! The controller presents decoded 901,120 byte ADF images as the raw
//! AmigaDOS MFM track stream Paula would DMA. Paula does not decode
//! sectors in hardware; ROM/trackdisk drivers read the MFM words into
//! chip RAM and decode them in software.

use crate::chipset::paula::PAULA_CLOCK_HZ;
use crate::config::{FloppyConfig, FloppyDriveConfig};
use crate::dms;
use anyhow::{bail, ensure, Context, Result};
use flate2::read::{DeflateDecoder, GzDecoder};
use flate2::CrcReader;
use log::{debug, warn};
use std::io::{Cursor, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

pub const CYLINDERS: usize = 80;
pub const SIDES: usize = 2;
pub const SECTORS_PER_TRACK: usize = 11;
pub const BYTES_PER_SECTOR: usize = 512;
pub const ADF_SIZE: usize = CYLINDERS * SIDES * SECTORS_PER_TRACK * BYTES_PER_SECTOR;
const MAX_EXTENDED_TRACKS: usize = 2 * 83;
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
const DISK_STATUS_SETTLE_CCK: u32 = PAULA_CLOCK_HZ / 1_000;
const INDEX_PULSE_CCK: u32 = PAULA_CLOCK_HZ / 250;
const INDEX_FLAG_SYNC_CCK: u32 = 1;
// HRM lists 3 ms step spacing and 18 ms direction-reversal spacing as drive
// programming requirements, but the CIA exposes only the STEP edge. Copperline
// moves the emulated head (and the /TRK0 sensor) on each edge immediately so
// recalibration -- which polls /TRK0 between fast step pulses -- never stalls
// (see cia_a_status_bits). What real hardware adds on top is a read-after-seek
// data-settle: while the head is physically traversing, the cells under it are
// not the destination track's data, so a trackloader that reads immediately
// after seeking (rather than waiting trackdisk's 15 ms) catches garbage until
// the head arrives, costing it up to a rotation of latency. We model that by
// holding off VALID read-data recovery for the head-move time after each step
// (longer on a direction reversal) while the platter keeps spinning, so the
// post-seek read resumes at a rotated position. Position sense (/TRK0) and
// motor/RDY are unaffected, so seeking and recalibration stay instant.
const SEEK_STEP_SETTLE_CCK: u32 = PAULA_CLOCK_HZ / 1_000 * 3; // ~3 ms per step
const SEEK_REVERSAL_SETTLE_CCK: u32 = PAULA_CLOCK_HZ / 1_000 * 18; // ~18 ms on reversal
                                                                   // 300 RPM.
const ROTATION_HZ: u32 = 5;
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
const IPF_SIGNATURE: &[u8; 4] = b"CAPS";
const SCP_SIGNATURE: &[u8; 3] = b"SCP";
const GZIP_SIGNATURE: &[u8; 2] = &[0x1F, 0x8B];
const ZIP_SIGNATURE: &[u8; 4] = &[0x50, 0x4b, 0x03, 0x04];
const STANDARD_EXTERNAL_DRIVE_ID: u32 = 0xFFFF_FFFF;
const SCP_TRACK_TABLE_OFFSET: usize = 0x10;
const SCP_EXTENDED_TRACK_TABLE_OFFSET: usize = 0x80;
const SCP_TRACK_TABLE_LEN: usize = SCP_TRACKS * 4;
const SCP_FLAG_INDEX: u8 = 1 << 0;
const SCP_FLAG_RPM_360: u8 = 1 << 2;
const SCP_FLAG_EXTENDED_MODE: u8 = 1 << 6;
const SCP_DEFAULT_16_BIT_FLUX_WIDTH: u8 = 0;
const SCP_EXPLICIT_16_BIT_FLUX_WIDTH: u8 = 16;
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

#[derive(serde::Serialize, serde::Deserialize)]
pub struct FloppyController {
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
    last_dskbytr_pos: Option<DiskBytePos>,
    last_stream_sync_pos: Option<DiskWordPos>,
    word_equal_latch: bool,
    sync_irq_latch: bool,
    index_pulse_cck: u32,
    index_flag_sync_cck: u32,
    index_flag_ready: bool,
    armed_dsklen: Option<u16>,
    dma: Option<DiskDma>,
    direct_write: Option<DiskDirectWrite>,
    dma_addr_mask: u32,
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
}

impl Default for FloppyController {
    fn default() -> Self {
        let mut drives: [FloppyDrive; 4] = std::array::from_fn(|_| FloppyDrive::default());
        for drive in drives.iter_mut().skip(1) {
            drive.external_id = 0;
        }
        Self {
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
            last_dskbytr_pos: None,
            last_stream_sync_pos: None,
            word_equal_latch: false,
            sync_irq_latch: false,
            index_pulse_cck: 0,
            index_flag_sync_cck: 0,
            index_flag_ready: false,
            armed_dsklen: None,
            dma: None,
            direct_write: None,
            dma_addr_mask: 0x001F_FFFF,
            sound_steps: 0,
            read_shifter: PaulaDiskReadDpllFifo::new(),
            // Idle at power-on; the first tick confirms it.
            idle_cache: true,
        }
    }
}

impl FloppyController {
    pub fn from_config(config: &FloppyConfig) -> Result<Self> {
        let mut ctrl = Self { ..Self::default() };
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
        for (idx, drive) in self.drives.iter_mut().enumerate().skip(1) {
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
        if !drive.rdy_line_asserted() {
            bits |= CIAA_DSKRDY;
        }
        bits
    }

    pub fn activity_led_on(&self) -> bool {
        self.selected_drive()
            .is_some_and(|idx| self.drives[idx].motor_on)
    }

    pub fn selected_track(&self) -> Option<u8> {
        self.selected_drive()
            .map(|idx| self.track_for_drive(idx) as u8)
    }

    /// Whether a drive is wired up: DF0 is the internal drive and always
    /// present; DF1-DF3 are present when they answer the external drive-ID
    /// protocol (configured drives get the standard ID, others read as no
    /// drive).
    pub fn drive_connected(&self, drive_idx: usize) -> bool {
        drive_idx == 0
            || self
                .drives
                .get(drive_idx)
                .is_some_and(|drive| drive.external_id != 0)
    }

    pub fn disk_inserted(&self, drive_idx: usize) -> bool {
        self.drives
            .get(drive_idx)
            .is_some_and(|drive| drive.image.is_some())
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

    pub fn eject_disk_image(&mut self, drive_idx: usize) -> Result<()> {
        ensure!(
            drive_idx < self.drives.len(),
            "invalid floppy drive df{}",
            drive_idx
        );
        self.idle_cache = false;
        self.drives[drive_idx].eject_image();
        Ok(())
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
        self.side = if val & CIAB_DSKSIDE == 0 { 1 } else { 0 };

        for idx in 0..self.drives.len() {
            let select_mask = CIAB_DSKSEL_MASKS[idx];
            let was_selected = prev & select_mask == 0;
            let selected = val & select_mask == 0;
            let select_activated = !was_selected && selected;
            let select_deactivated = was_selected && !selected;

            if idx == 0 {
                if selected {
                    let motor_on = val & CIAB_DSKMOTOR == 0;
                    self.drives[idx].set_motor(motor_on);
                }
            } else if select_activated {
                let motor_on = val & CIAB_DSKMOTOR == 0;
                self.drives[idx].latch_mtrxd(motor_on);
            } else if select_deactivated {
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
                self.drives[idx].step(inward);
                self.handle_active_dma_track_change(idx);
                // Every step pulse moves the head mechanism audibly,
                // including trackdisk's no-disk change-line polling
                // (the classic empty-drive click) and bump-stops at
                // the ends of travel.
                self.sound_steps = self.sound_steps.saturating_add(1);
            }

            if was_selected != selected {
                debug!("floppy.df{} selected={}", idx, selected);
            }
        }
        if let Some(idx) = self.selected_drive() {
            self.ensure_track(idx, self.track_for_drive(idx));
        }
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
        self.word_equal_latch = false;
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

    pub fn read_dskbytr(&mut self, dmacon: u16, adkcon: u16) -> u16 {
        let mut status = 0u16;
        if self.dma_enabled(dmacon) {
            status |= DMAON;
        }
        if self.dsklen & DSKLEN_WRITE != 0 {
            status |= DISKWRITE;
        }
        let dskbytr_load_allowed = self.dsklen & (DSKLEN_DMAEN | DSKLEN_WRITE) != DSKLEN_WRITE;
        let active_write_dma = self.dma.as_ref().is_some_and(|dma| dma.write);
        let mut current_word = None;
        let mut new_disk_word = false;
        if let Some((idx, track)) = self.selected_ready_track() {
            self.ensure_track(idx, track);
            let drive = &self.drives[idx];
            if let Some(rev) = drive.cur_rev() {
                let bit = drive.rotation_bit;
                let byte_index = bit / 8;
                let word_index = bit / 16;
                let word = rev.word_at(word_index * 16);
                let byte = rev.byte_at(byte_index * 8);
                let byte_pos = DiskBytePos {
                    drive: idx,
                    track,
                    word: byte_index,
                    byte_phase: 0,
                };
                let word_pos = DiskWordPos {
                    drive: idx,
                    track,
                    word: word_index,
                };
                current_word = Some(word);
                new_disk_word = self.last_stream_sync_pos != Some(word_pos);
                self.last_stream_sync_pos = Some(word_pos);
                if dskbytr_load_allowed && self.last_dskbytr_pos != Some(byte_pos) {
                    if active_write_dma {
                        self.last_dskbytr_byte = 0;
                    } else {
                        self.last_dskdatr = word;
                        self.last_dskbytr_byte = byte;
                    }
                    self.dskbyte_valid = true;
                    self.last_dskbytr_pos = Some(byte_pos);
                }
            }
        } else {
            self.last_dskbytr_pos = None;
            self.last_stream_sync_pos = None;
        }
        let current_word_equal = current_word.is_some_and(|word| word == self.dsksync);
        let sync_irq_allowed = adkcon & ADK_MSBSYNC == 0 && !active_write_dma;
        if current_word_equal && new_disk_word && sync_irq_allowed {
            self.record_sync_match();
        }
        if self.word_equal_latch || current_word_equal {
            status |= WORDEQUAL;
        }
        if self.dskbyte_valid {
            status |= DSKBYT;
            self.dskbyte_valid = false;
        }
        self.word_equal_latch = false;
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

        match active_dma {
            Some((dma_idx, _, true)) if dma_idx == idx => {
                self.tick_write_dma(idx, cck, is_selected, chip_ram)
            }
            _ => self.tick_read_and_rotate(idx, cck, dmacon, is_selected, chip_ram),
        }
    }

    /// Advance the reading drive's head one MFM cell at a time at the recovered
    /// per-cell rate, feeding each cell to Paula's read shifter. Handles the
    /// live read DMA (bit-aligned sync wait, sync-framed word transfer) and the
    /// free-running sync comparator / DSKSYNC interrupt.
    fn tick_read_and_rotate(
        &mut self,
        idx: usize,
        cck: u32,
        dmacon: u16,
        is_selected: bool,
        chip_ram: &mut [u8],
    ) -> bool {
        if !self.drives[idx].motor_on || self.drives[idx].cached.is_empty() {
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

            if self.drives[idx].advance_head_bit() {
                index_pulse = true;
            }

            if let Some(dma) = read_dma.as_mut() {
                if !dma.wait_sync {
                    while let Some(word) = self.read_shifter.read_fifo_word() {
                        if dma.remaining == 0 {
                            break;
                        }
                        write_chip_word(chip_ram, self.dskpt, word);
                        self.last_dskdatr = word;
                        self.last_dskbytr_byte = (word & 0x00FF) as u8;
                        self.dskbyte_valid = true;
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
        is_selected: bool,
        chip_ram: &mut [u8],
    ) -> bool {
        if !self.drives[idx].motor_on || self.drives[idx].cached.is_empty() {
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
            let word = read_chip_word(chip_ram, self.dskpt);
            dma.write_words.push(word);
            self.advance_dskpt();
            for _ in 0..16 {
                if self.drives[idx].advance_head_bit() {
                    index_pulse = true;
                }
            }
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

    pub fn next_completion_cck(&self, dmacon: u16) -> Option<u32> {
        let dma = self.dma.as_ref()?;
        if !self.dma_enabled(dmacon) || dma.wait_sync {
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
        Some((drive.head_cck_for_bits(bits).min(u64::from(u32::MAX)) as u32).max(1))
    }

    pub fn next_index_pulse_cck(&self) -> Option<u32> {
        if self.index_flag_sync_cck != 0 {
            return Some(self.index_flag_sync_cck);
        }
        let idx = self.selected_drive()?;
        let drive = &self.drives[idx];
        if drive.image.is_none() || !drive.motor_on {
            return None;
        }
        let rev = drive.cur_rev()?;
        // A single-word (or shorter) track is too short to advertise an index.
        if rev.bit_len <= 16 {
            return None;
        }
        let bits_to_end = rev.bit_len - (drive.rotation_bit % rev.bit_len);
        Some(
            (drive
                .head_cck_for_bits(bits_to_end)
                .min(u64::from(u32::MAX)) as u32)
                .max(1),
        )
    }

    pub fn dma_active(&self, dmacon: u16) -> bool {
        self.dma_enabled(dmacon)
    }

    /// Drain the head-step pulses accumulated since the last call,
    /// for the synthesized drive sound effects.
    pub fn take_sound_steps(&mut self) -> u32 {
        std::mem::take(&mut self.sound_steps)
    }

    /// Per-drive platter spin level for the drive sound effects: 0.0
    /// stopped to 1.0 at full speed. Rides the motor spin-up/spin-down
    /// accumulator, so the audible motor glides over the real ~0.5 s
    /// ramp instead of switching.
    pub fn motor_spin_levels(&self) -> [f32; 4] {
        std::array::from_fn(|idx| {
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
            return self.no_drive_completion();
        };
        if !self.drives[idx].ready() {
            return self.no_drive_completion();
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
        });
        debug!(
            "floppy DMA start df{} track={} write={} words={} sync_wait={} msb_sync={}",
            idx, track, write, remaining, word_sync, msb_sync
        );
        if crate::envcfg::flag("COPPERLINE_DIAG_DISK") {
            let secs = self.drives[idx].elapsed_cck as f64 / PAULA_CLOCK_HZ as f64;
            log::info!(
                "disk-dma secs={secs:.5} df{idx} track={track} cyl={} write={write} words={remaining} rotbit={}",
                self.drives[idx].cylinder,
                self.drives[idx].rotation_bit,
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
        let write_result: Result<()> = match &mut image.data {
            FloppyImageData::StandardAdf(image_data) => {
                decode_non_empty_track_write(track, write_words).and_then(|sectors| {
                    apply_standard_adf_sectors(image_data, track, &sectors);
                    write_standard_adf_sectors_to_disk(&path, track, &sectors)
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
            .and_then(|encoded| {
                std::fs::write(&path, encoded).context("writing extended ADF image")
            }),
        };
        match write_result {
            Ok(()) => {
                drive.cached_track = None;
                debug!("floppy.df{} write-through complete", drive_idx);
            }
            Err(e) => warn!("floppy.df{} write-through failed: {e:#}", drive_idx),
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
            .position(|select_mask| self.prb & select_mask == 0)
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

    fn record_sync_match(&mut self) {
        self.word_equal_latch = true;
        self.sync_irq_latch = true;
    }

    fn ensure_track(&mut self, idx: usize, track: usize) {
        let drive = &mut self.drives[idx];
        if drive.cached_track == Some(track) {
            return;
        }
        drive.cached = CachedTrack::default();
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
            image: None,
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

    fn ready(&self) -> bool {
        self.image.is_some() && self.motor_on && self.motor_cck >= MOTOR_READY_CCK
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
        self.status_settle_cck = DISK_STATUS_SETTLE_CCK;
    }

    fn set_motor(&mut self, on: bool) {
        if self.motor_on == on {
            return;
        }
        self.motor_on = on;
        // Disk rotational inertia: a motor-off does not stop the platter
        // instantly. The spin-up accumulator is preserved here and decays
        // only while the motor stays off (see tick_motor). This matches real
        // drives, where /RDY survives the brief motor toggles some
        // trackloaders (e.g. Magic Pockets) issue between sector reads.
    }

    fn step(&mut self, inward: bool) {
        let previous = self.cylinder;
        self.cylinder = if inward {
            self.cylinder.saturating_add(1).min((CYLINDERS - 1) as u8)
        } else {
            self.cylinder.saturating_sub(1)
        };
        if self.image.is_some() {
            self.set_disk_change(false);
        }
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
#[derive(Clone, Default, serde::Serialize, serde::Deserialize)]
struct TrackRev {
    words: Vec<u16>,
    bit_len: usize,
    word_cck: u32,
}

impl TrackRev {
    fn new(words: Vec<u16>, bit_len: usize, word_cck: u32) -> Self {
        let bit_len = bit_len.min(words.len() * 16);
        Self {
            words,
            bit_len,
            word_cck: word_cck.max(1),
        }
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

    /// The 8-bit byte starting at `bit` (MSB-first), wrapping at `bit_len`.
    fn byte_at(&self, bit: usize) -> u8 {
        let mut value = 0u8;
        for offset in 0..8 {
            value = (value << 1) | u8::from(self.bit(bit + offset));
        }
        value
    }

    /// Cumulative cck from the start of the revolution to the start of `bit`.
    /// `prefix(16k) == k*word_cck` exactly, so aligned word boundaries match
    /// the old uniform word clock.
    fn prefix_cck(&self, bit: usize) -> u64 {
        (bit as u64 * self.word_cck as u64 + 8) / 16
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
    fn bits_until_sync(&self, from: usize, sync: u16) -> Option<usize> {
        if self.bit_len == 0 {
            return None;
        }
        let mut window = 0u16;
        // Prime the 15 bits before `from` so the first compared window ends at
        // `from` (the bit about to be read).
        for i in 0..15 {
            let b = (from + self.bit_len - 15 + i) % self.bit_len;
            window = (window << 1) | u16::from(self.bit(b));
        }
        for step in 0..self.bit_len {
            let b = (from + step) % self.bit_len;
            window = (window << 1) | u16::from(self.bit(b));
            if window == sync {
                return Some(step + 1);
            }
        }
        None
    }
}

#[derive(Clone, Default, serde::Serialize, serde::Deserialize)]
struct CachedTrack {
    revs: Vec<TrackRev>,
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

#[derive(serde::Serialize, serde::Deserialize)]
struct FloppyImage {
    path: PathBuf,
    data: FloppyImageData,
    write_protected: bool,
    legacy_extended_adf: bool,
}

#[derive(serde::Serialize, serde::Deserialize)]
enum FloppyImageData {
    StandardAdf(Vec<u8>),
    Tracks(Vec<Option<FloppyTrackImage>>),
}

#[derive(serde::Serialize, serde::Deserialize)]
enum FloppyTrackImage {
    AmigaDos(Vec<u8>),
    RawMfm {
        words: Vec<u16>,
        bit_len: u32,
        stored_len: usize,
        revolutions: u8,
        legacy_sync: Option<u16>,
        bitcell_ns: Option<Vec<u32>>,
    },
}

impl FloppyImage {
    fn load(config: &FloppyDriveConfig) -> Result<Self> {
        let packed = std::fs::read(&config.path)
            .with_context(|| format!("reading floppy image {}", config.path.display()))?;
        let (data, write_protected, legacy_extended_adf) = if packed.starts_with(GZIP_SIGNATURE) {
            let unpacked = decode_gzip_floppy_image(&packed)?;
            decode_floppy_payload(unpacked, true, &config.path)?
        } else if packed.starts_with(ZIP_SIGNATURE) {
            let unpacked = decode_zip_floppy_image(&packed)?;
            decode_floppy_payload(unpacked, true, &config.path)?
        } else {
            decode_floppy_payload(packed, config.write_protected, &config.path)?
        };

        Ok(Self {
            path: config.path.clone(),
            data,
            write_protected,
            legacy_extended_adf,
        })
    }

    fn track_stream(&self, track: usize) -> Option<TrackStream> {
        match &self.data {
            FloppyImageData::StandardAdf(adf) => {
                Some(synthetic_track_stream(encode_adf_track(track, adf)))
            }
            FloppyImageData::Tracks(tracks) => {
                let image_track = tracks.get(track)?.as_ref()?;
                match image_track {
                    FloppyTrackImage::AmigaDos(data) => {
                        Some(synthetic_track_stream(encode_amigados_track(track, data)))
                    }
                    FloppyTrackImage::RawMfm {
                        words,
                        bit_len,
                        revolutions,
                        legacy_sync,
                        ..
                    } => Some(raw_mfm_track_stream(
                        words,
                        *bit_len,
                        *revolutions,
                        legacy_sync.is_some(),
                    )),
                }
            }
        }
    }
}

/// A synthetic (ADF / AmigaDOS) track: one perfectly-aligned revolution of
/// uniform 2 us cells. The uniform `word_cck` keeps the DMA word cadence and
/// every timing assertion identical to the old word-grid model.
fn synthetic_track_stream(words: Vec<u16>) -> TrackStream {
    let word_cck = FloppyController::word_cck_for_track_words(words.len());
    let bit_len = words.len() * 16;
    TrackStream {
        revs: vec![TrackRev::new(words, bit_len, word_cck)],
    }
}

/// A flux/raw-MFM track: split the stored stream into its captured revolutions,
/// each with its exact `bit_len`, so the looping head sees no word-rounding
/// seam at the index and weak/fuzzy bits vary per revolution.
fn raw_mfm_track_stream(
    words: &[u16],
    bit_len: u32,
    revolutions: u8,
    legacy_sync: bool,
) -> TrackStream {
    let rev_bits = (bit_len as usize).max(1);
    let words_per_rev = rev_bits.div_ceil(16).max(1);
    let rev_count = if legacy_sync {
        1
    } else {
        (revolutions.max(1) as usize)
            .min(words.len() / words_per_rev)
            .max(1)
    };

    let mut revs = Vec::with_capacity(rev_count);
    for r in 0..rev_count {
        let start = r * words_per_rev;
        let end = (start + words_per_rev).min(words.len());
        if start >= end {
            break;
        }
        let rev_words = words[start..end].to_vec();
        let this_bits = rev_bits.min(rev_words.len() * 16);
        let word_cck = FloppyController::word_cck_for_track_words(rev_words.len());
        revs.push(TrackRev::new(rev_words, this_bits, word_cck));
    }
    if revs.is_empty() {
        let word_cck = FloppyController::word_cck_for_track_words(words.len());
        revs.push(TrackRev::new(words.to_vec(), words.len() * 16, word_cck));
    }
    TrackStream { revs }
}

fn decode_floppy_payload(
    packed: Vec<u8>,
    config_write_protected: bool,
    path: &Path,
) -> Result<(FloppyImageData, bool, bool)> {
    let legacy_extended_adf = packed.starts_with(UAE_EXT1_SIGNATURE);
    let (data, write_protected) = if packed.len() == ADF_SIZE {
        (FloppyImageData::StandardAdf(packed), config_write_protected)
    } else if packed.starts_with(UAE_EXT2_SIGNATURE) {
        (decode_uae_extended_adf(&packed)?, config_write_protected)
    } else if packed.starts_with(UAE_EXT1_SIGNATURE) {
        (
            decode_uae_legacy_extended_adf(&packed)?,
            config_write_protected,
        )
    } else if dms::is_dms(&packed) {
        let data = dms::decode_dms_adf(&packed)
            .with_context(|| format!("decoding DMS {}", path.display()))?;
        (FloppyImageData::StandardAdf(data), true)
    } else if packed.starts_with(IPF_SIGNATURE) {
        bail!(
            "IPF/CAPS floppy image {} is not supported: Copperline treats IPF as an explicit non-goal for the built-in loader until support is implemented as a direct IPF parser or optional SPS/CAPS library integration with licensing and platform packaging reviewed",
            path.display()
        );
    } else if packed.starts_with(SCP_SIGNATURE) {
        (decode_scp_flux_image(&packed)?, true)
    } else {
        bail!(
            "floppy image {} is {} bytes; expected {} bytes (ADF), gzip-compressed supported image, UAE extended ADF, SCP, or DMS",
            path.display(),
            packed.len(),
            ADF_SIZE
        );
    };
    Ok((data, write_protected, legacy_extended_adf))
}

fn decode_gzip_floppy_image(data: &[u8]) -> Result<Vec<u8>> {
    ensure!(data.starts_with(GZIP_SIGNATURE), "missing gzip signature");
    let mut decoder = GzDecoder::new(data);
    let mut unpacked = Vec::with_capacity(ADF_SIZE);
    decoder
        .read_to_end(&mut unpacked)
        .context("decompressing gzip-compressed floppy image")?;
    Ok(unpacked)
}

fn decode_zip_floppy_image(data: &[u8]) -> Result<Vec<u8>> {
    ensure!(data.starts_with(ZIP_SIGNATURE), "missing zip signature");
    let mut cursor = Cursor::new(data);
    let mut u16buf = [0u8; 2];
    let mut u32buf = [0u8; 4];

    // Skip to compression method (at offset 8 in local file header)
    cursor.set_position(8);
    cursor.read_exact(&mut u16buf)?;
    let compression = u16::from_le_bytes(u16buf);

    // Skip to CRC-32 (at offset 14 in local file header)
    cursor.set_position(14);
    cursor.read_exact(&mut u32buf)?;
    let expected_crc = u32::from_le_bytes(u32buf);

    // Skip to uncompressed size (at offset 22 in local file header)
    cursor.set_position(22);
    cursor.read_exact(&mut u32buf)?;
    let uncomp_size = u32::from_le_bytes(u32buf);
    ensure!(
        uncomp_size as usize == ADF_SIZE,
        "invalid ADF file size in ZIP archive"
    );

    // Skip to file name length and extra field length
    cursor.set_position(26);
    cursor.read_exact(&mut u16buf)?;
    let file_name_length = u16::from_le_bytes(u16buf);
    cursor.read_exact(&mut u16buf)?;
    let extra_field_length = u16::from_le_bytes(u16buf);

    // Skip the file name and extra field to reach the compressed data
    cursor.set_position((30 + file_name_length + extra_field_length) as u64);

    let mut decompressed = vec![0; ADF_SIZE];

    let calculated_crc = match compression {
        8 => {
            // Deflate compression
            let mut decode_reader = CrcReader::new(DeflateDecoder::new(cursor));
            decode_reader
                .read_exact(&mut decompressed)
                .context("deflating zipped floppy image")?;
            decode_reader.crc().sum()
        }
        0 => {
            // No compression
            let mut reader = CrcReader::new(cursor);
            reader
                .read_exact(&mut decompressed)
                .context("unarchiving zipped floppy image")?;
            reader.crc().sum()
        }
        n => {
            bail!("Unsupported compression method in zip archive: {n}");
        }
    };
    // Verify CRC32
    if calculated_crc != expected_crc {
        bail!("checksum error in zip archive: expected: {expected_crc} != calculated: {calculated_crc}");
    }

    Ok(decompressed)
}

fn decode_uae_extended_adf(data: &[u8]) -> Result<FloppyImageData> {
    ensure!(data.len() >= 12, "UAE extended ADF header is truncated");
    ensure!(
        data.starts_with(UAE_EXT2_SIGNATURE),
        "missing UAE-1ADF signature"
    );
    let tracks = u16::from_be_bytes([data[10], data[11]]) as usize;
    ensure!(
        tracks <= MAX_EXTENDED_TRACKS,
        "UAE extended ADF has {tracks} tracks, max supported is {MAX_EXTENDED_TRACKS}"
    );
    let header_len = 12 + tracks * 12;
    ensure!(
        data.len() >= header_len,
        "UAE extended ADF track table is truncated"
    );

    let mut offset = header_len;
    let mut out = Vec::with_capacity(tracks);
    for track in 0..tracks {
        let desc = &data[12 + track * 12..12 + (track + 1) * 12];
        let revolutions = desc[2].saturating_add(1);
        let track_type = desc[3];
        let len = u32::from_be_bytes([desc[4], desc[5], desc[6], desc[7]]) as usize;
        let bit_len = u32::from_be_bytes([desc[8], desc[9], desc[10], desc[11]]);
        ensure!(
            offset + len <= data.len(),
            "UAE extended ADF track {track} data is truncated"
        );
        let payload = &data[offset..offset + len];
        offset += len;

        let image_track = match track_type {
            0 => {
                if len == 0 {
                    None
                } else {
                    decode_uae_extended_amigados_payload(track, payload, bit_len)?
                        .map(|sector_data| FloppyTrackImage::AmigaDos(sector_data.to_vec()))
                }
            }
            1 => Some(FloppyTrackImage::RawMfm {
                words: raw_mfm_words(track, payload, bit_len)?,
                bit_len: if bit_len == 0 {
                    (len * 8) as u32
                } else {
                    bit_len
                },
                stored_len: len,
                revolutions,
                legacy_sync: None,
                bitcell_ns: None,
            }),
            other => {
                ensure!(
                    len == 0,
                    "unsupported UAE extended ADF track {track} type {other}"
                );
                None
            }
        };
        if revolutions > 1 && matches!(image_track, Some(FloppyTrackImage::RawMfm { .. })) {
            debug!(
                "UAE extended ADF raw track {track} has {revolutions} stored revolutions; preserving cyclic raw stream"
            );
        }
        out.push(image_track);
    }
    Ok(FloppyImageData::Tracks(out))
}

fn decode_uae_extended_amigados_payload(
    track: usize,
    payload: &[u8],
    bit_len: u32,
) -> Result<Option<&[u8]>> {
    let data_len = if bit_len == 0 {
        payload.len()
    } else {
        ensure!(
            bit_len.is_multiple_of(8),
            "UAE extended ADF track {track} AmigaDOS bit length is not byte-aligned"
        );
        (bit_len / 8) as usize
    };
    ensure!(
        data_len <= payload.len(),
        "UAE extended ADF track {track} AmigaDOS bit length exceeds stored data"
    );
    // UAE-1ADF exporters can store type-0 DOS tracks at raw-track byte
    // length. `bit_len` identifies the sector payload; any surplus is fill.
    // Extra blank cylinders may also appear as all-zero raw-length type-0
    // tracks, which carry no sector stream.
    if !data_len.is_multiple_of(BYTES_PER_SECTOR) && payload.iter().all(|&byte| byte == 0) {
        return Ok(None);
    }
    ensure!(
        data_len.is_multiple_of(BYTES_PER_SECTOR),
        "UAE extended ADF track {track} AmigaDOS data is not sector-aligned"
    );
    ensure!(
        payload[data_len..].iter().all(|&byte| byte == 0),
        "UAE extended ADF track {track} AmigaDOS padding after bit length is non-zero"
    );
    Ok(Some(&payload[..data_len]))
}

fn decode_uae_legacy_extended_adf(data: &[u8]) -> Result<FloppyImageData> {
    ensure!(data.len() >= 8 + 160 * 4, "UAE--ADF header is truncated");
    ensure!(
        data.starts_with(UAE_EXT1_SIGNATURE),
        "missing UAE--ADF signature"
    );
    let mut offset = 8 + 160 * 4;
    let mut out = Vec::with_capacity(160);
    for track in 0..160 {
        let desc = &data[8 + track * 4..8 + (track + 1) * 4];
        let sync = u16::from_be_bytes([desc[0], desc[1]]);
        let len = u16::from_be_bytes([desc[2], desc[3]]) as usize;
        ensure!(
            offset + len <= data.len(),
            "UAE--ADF track {track} data is truncated"
        );
        let payload = &data[offset..offset + len];
        offset += len;
        if len == 0 {
            out.push(None);
        } else if sync == 0 {
            ensure!(
                len.is_multiple_of(BYTES_PER_SECTOR),
                "UAE--ADF track {track} AmigaDOS data is not sector-aligned"
            );
            out.push(Some(FloppyTrackImage::AmigaDos(payload.to_vec())));
        } else {
            let mut words = Vec::with_capacity(len / 2 + 1);
            words.push(sync);
            words.extend(raw_mfm_words(track, payload, (len * 8) as u32)?);
            out.push(Some(FloppyTrackImage::RawMfm {
                words,
                bit_len: (len * 8 + 16) as u32,
                stored_len: len,
                revolutions: 1,
                legacy_sync: Some(sync),
                bitcell_ns: None,
            }));
        }
    }
    Ok(FloppyImageData::Tracks(out))
}

fn decode_scp_flux_image(data: &[u8]) -> Result<FloppyImageData> {
    ensure!(data.len() >= 0x10, "SCP image header is truncated");
    ensure!(data.starts_with(SCP_SIGNATURE), "missing SCP signature");
    let flags = data[0x08];
    verify_scp_checksum(data)?;
    ensure!(
        scp_flux_width_is_16_bit(data[0x09]),
        "SCP flux entry width {} is not supported",
        data[0x09]
    );
    let track_table_offset = scp_track_table_offset(flags);
    ensure!(
        data.len() >= track_table_offset + SCP_TRACK_TABLE_LEN,
        "SCP track header table is truncated"
    );

    let revolutions = data[0x05] as usize;
    ensure!(revolutions > 0, "SCP image has no revolutions");
    let start_track = data[0x06] as usize;
    let end_track = data[0x07] as usize;
    ensure!(
        start_track <= end_track && end_track < SCP_TRACKS,
        "SCP track range {start_track}..={end_track} is invalid"
    );
    let flux_resolution_ns = SCP_CAPTURE_BASE_NS;

    let mut tracks: Vec<Option<FloppyTrackImage>> = (0..SCP_TRACKS).map(|_| None).collect();
    for track in start_track..=end_track {
        let table_off = track_table_offset + track * 4;
        let tdh_offset = read_le_u32(&data[table_off..table_off + 4]) as usize;
        if tdh_offset == 0 {
            continue;
        }
        ensure!(
            tdh_offset < data.len(),
            "SCP track {track} header offset is outside the image"
        );
        tracks[track] = Some(
            decode_scp_track(
                data,
                track,
                tdh_offset,
                revolutions,
                flags,
                flux_resolution_ns,
            )
            .with_context(|| format!("decoding SCP track {track}"))?,
        );
    }

    Ok(FloppyImageData::Tracks(tracks))
}

fn scp_flux_width_is_16_bit(width: u8) -> bool {
    matches!(
        width,
        SCP_DEFAULT_16_BIT_FLUX_WIDTH | SCP_EXPLICIT_16_BIT_FLUX_WIDTH
    )
}

fn verify_scp_checksum(data: &[u8]) -> Result<()> {
    let expected = read_le_u32(&data[SCP_CHECKSUM_OFFSET..SCP_CHECKSUM_OFFSET + 4]);
    if expected == 0 {
        return Ok(());
    }
    let actual = scp_checksum(data);
    ensure!(
        actual == expected,
        "SCP checksum mismatch: expected {expected:08X}, got {actual:08X}"
    );
    Ok(())
}

fn scp_checksum(data: &[u8]) -> u32 {
    data.get(SCP_CHECKSUM_START..)
        .unwrap_or_default()
        .iter()
        .fold(0u32, |sum, &byte| sum.wrapping_add(u32::from(byte)))
}

fn scp_track_table_offset(flags: u8) -> usize {
    if flags & SCP_FLAG_EXTENDED_MODE != 0 {
        SCP_EXTENDED_TRACK_TABLE_OFFSET
    } else {
        SCP_TRACK_TABLE_OFFSET
    }
}

fn decode_scp_track(
    data: &[u8],
    track: usize,
    tdh_offset: usize,
    revolutions: usize,
    flags: u8,
    flux_resolution_ns: u64,
) -> Result<FloppyTrackImage> {
    let header_len = 4 + revolutions * 12;
    let header_end = tdh_offset
        .checked_add(header_len)
        .context("SCP track header offset overflow")?;
    ensure!(
        header_end <= data.len(),
        "SCP track {track} header is truncated"
    );
    let header = &data[tdh_offset..header_end];
    ensure!(
        &header[0..3] == b"TRK",
        "SCP track {track} is missing TRK header"
    );
    ensure!(
        header[3] as usize == track,
        "SCP track header number {} does not match table entry {track}",
        header[3]
    );

    let mut target_bit_len = None;
    let mut decoded_revolutions = 0u8;
    let mut all_words = Vec::new();
    let mut all_bitcell_ns = Vec::new();
    for rev in 0..revolutions {
        let entry = 4 + rev * 12;
        let index_time = read_le_u32(&header[entry..entry + 4]);
        let flux_entries = read_le_u32(&header[entry + 4..entry + 8]);
        let data_offset = read_le_u32(&header[entry + 8..entry + 12]) as usize;
        if flux_entries == 0 {
            continue;
        }
        ensure!(
            data_offset >= header_len,
            "SCP track {track} revolution {rev} flux data overlaps the track header"
        );

        let flux_bytes = (flux_entries as usize)
            .checked_mul(2)
            .context("SCP flux data length overflow")?;
        let flux_start = tdh_offset
            .checked_add(data_offset)
            .context("SCP flux data offset overflow")?;
        let flux_end = flux_start
            .checked_add(flux_bytes)
            .context("SCP flux data end overflow")?;
        ensure!(
            flux_end <= data.len(),
            "SCP track {track} revolution {rev} flux data is truncated"
        );

        let rev_target = match target_bit_len {
            Some(bits) => Some(bits),
            None => scp_revolution_bit_len(index_time, flags)?,
        };
        let (words, bit_len, bitcell_ns) = scp_flux_to_mfm_words(
            track,
            rev,
            &data[flux_start..flux_end],
            flux_resolution_ns,
            rev_target,
        )?;
        let target = *target_bit_len.get_or_insert(bit_len);
        ensure!(
            bit_len == target,
            "SCP track {track} revolution {rev} bit length {bit_len} does not match first revolution {target}"
        );
        all_words.extend(words);
        all_bitcell_ns.extend(bitcell_ns);
        decoded_revolutions = decoded_revolutions.saturating_add(1);
    }

    ensure!(
        decoded_revolutions > 0,
        "SCP track {track} has no flux data"
    );
    let bit_len = target_bit_len.unwrap_or(0);
    let stored_len = all_words.len() * 2;
    Ok(FloppyTrackImage::RawMfm {
        words: all_words,
        bit_len,
        stored_len,
        revolutions: decoded_revolutions,
        legacy_sync: None,
        bitcell_ns: Some(all_bitcell_ns),
    })
}

fn scp_revolution_bit_len(index_time: u32, flags: u8) -> Result<Option<u32>> {
    if flags & SCP_FLAG_INDEX == 0 {
        let ns = if flags & SCP_FLAG_RPM_360 != 0 {
            SCP_360_RPM_REV_NS
        } else {
            SCP_300_RPM_REV_NS
        };
        return scp_bit_len_from_ns(ns).map(Some);
    }

    if index_time == 0 {
        Ok(None)
    } else {
        scp_bit_len_from_ns(u64::from(index_time) * SCP_CAPTURE_BASE_NS).map(Some)
    }
}

fn scp_bit_len_from_ns(ns: u64) -> Result<u32> {
    let bits = ((ns + AMIGA_DD_BITCELL_NS / 2) / AMIGA_DD_BITCELL_NS).max(1);
    ensure!(
        bits <= u64::from(MAX_SCP_REVOLUTION_BITS),
        "SCP revolution bit length {bits} exceeds supported limit {MAX_SCP_REVOLUTION_BITS}"
    );
    Ok(bits as u32)
}

fn scp_flux_to_mfm_words(
    track: usize,
    rev: usize,
    flux: &[u8],
    flux_resolution_ns: u64,
    target_bit_len: Option<u32>,
) -> Result<(Vec<u16>, u32, Vec<u32>)> {
    ensure!(
        flux.len().is_multiple_of(2),
        "SCP track {track} revolution {rev} has odd flux byte length"
    );
    let bit_cap = target_bit_len.unwrap_or(MAX_SCP_REVOLUTION_BITS);
    let capped_by_index = target_bit_len.is_some();
    let mut words = Vec::new();
    let mut bitcell_ns = Vec::new();
    let mut bit_len = 0u32;
    let mut overflow_ticks = 0u64;
    // PLL data separator: recover MFM cells from flux intervals, locking the
    // cell-time estimate onto the local flux rate. For each interval the cell
    // count is `round(interval / cell)`; the estimate is then nudged toward the
    // measured per-cell time. A flux transition is a "1" cell preceded by
    // (n-1) "0" cells. This avoids the cumulative drift a fixed 2 us grid
    // accumulates when the disk's true rate differs from nominal.
    let mut cell_ns = AMIGA_DD_BITCELL_NS as f64;
    for chunk in flux.chunks_exact(2) {
        let ticks = u64::from(read_be_u16(chunk));
        if ticks == 0 {
            overflow_ticks = overflow_ticks.saturating_add(65_536);
            continue;
        }
        let total_ticks = overflow_ticks.saturating_add(ticks);
        overflow_ticks = 0;
        let interval_ns = total_ticks
            .checked_mul(flux_resolution_ns)
            .context("SCP flux interval overflows nanoseconds")? as f64;
        let cells = (interval_ns / cell_ns).round().max(1.0);
        let measured = interval_ns / cells;
        cell_ns += (measured - cell_ns) * SCP_PLL_GAIN;
        cell_ns = cell_ns.clamp(SCP_PLL_MIN_CELL_NS, SCP_PLL_MAX_CELL_NS);
        let per_cell_ns = measured.round().clamp(1.0, u32::MAX as f64) as u32;
        let cells = cells as u64;
        append_scp_cells(
            &mut words,
            &mut bitcell_ns,
            &mut bit_len,
            cells.saturating_sub(1),
            false,
            bit_cap,
            capped_by_index,
            per_cell_ns,
        )
        .with_context(|| format!("SCP track {track} revolution {rev} flux interval"))?;
        append_scp_cells(
            &mut words,
            &mut bitcell_ns,
            &mut bit_len,
            1,
            true,
            bit_cap,
            capped_by_index,
            per_cell_ns,
        )
        .with_context(|| format!("SCP track {track} revolution {rev} flux interval"))?;
    }
    if overflow_ticks != 0 && !capped_by_index {
        // Trailing no-flux gap before the index hole: pad with idle cells at
        // the current recovered rate.
        let interval_ns = overflow_ticks
            .checked_mul(flux_resolution_ns)
            .context("SCP flux silence overflows nanoseconds")? as f64;
        let cells = (interval_ns / cell_ns).round().max(0.0) as u64;
        append_scp_cells(
            &mut words,
            &mut bitcell_ns,
            &mut bit_len,
            cells,
            false,
            bit_cap,
            capped_by_index,
            cell_ns.round() as u32,
        )
        .with_context(|| format!("SCP track {track} revolution {rev} trailing flux overflow"))?;
    }
    if let Some(target) = target_bit_len {
        // SCP gives no per-cell timing after the last transition; the
        // synthetic index-padding cells retain nominal DD timing.
        let padding_cells = u64::from(target.saturating_sub(bit_len));
        append_scp_cells(
            &mut words,
            &mut bitcell_ns,
            &mut bit_len,
            padding_cells,
            false,
            target,
            true,
            AMIGA_DD_BITCELL_NS as u32,
        )?;
    }
    ensure!(
        bit_len > 0,
        "SCP track {track} revolution {rev} produced an empty bit stream"
    );
    Ok((words, bit_len, bitcell_ns))
}

fn append_scp_cells(
    words: &mut Vec<u16>,
    bitcell_ns: &mut Vec<u32>,
    bit_len: &mut u32,
    cells: u64,
    bit: bool,
    bit_cap: u32,
    capped_by_index: bool,
    cell_ns: u32,
) -> Result<()> {
    let available = u64::from(bit_cap.saturating_sub(*bit_len));
    if cells > available && !capped_by_index {
        bail!("SCP flux stream exceeds supported bit length {MAX_SCP_REVOLUTION_BITS}");
    }
    for _ in 0..cells.min(available) {
        push_mfm_bit(words, bit_len, bit);
        bitcell_ns.push(cell_ns);
    }
    Ok(())
}

fn push_mfm_bit(words: &mut Vec<u16>, bit_len: &mut u32, bit: bool) {
    if (*bit_len).is_multiple_of(16) {
        words.push(0);
    }
    if bit {
        let bit_pos = 15 - (*bit_len % 16);
        if let Some(word) = words.last_mut() {
            *word |= 1 << bit_pos;
        }
    }
    *bit_len = bit_len.saturating_add(1);
}

fn read_le_u32(bytes: &[u8]) -> u32 {
    u32::from_le_bytes(bytes.try_into().unwrap())
}

fn read_be_u16(bytes: &[u8]) -> u16 {
    u16::from_be_bytes(bytes.try_into().unwrap())
}

fn raw_mfm_words(track: usize, payload: &[u8], bit_len: u32) -> Result<Vec<u16>> {
    let effective_bit_len = if bit_len == 0 {
        (payload.len() * 8) as u32
    } else {
        bit_len
    };
    ensure!(
        effective_bit_len as usize <= payload.len() * 8,
        "raw MFM track {track} bit length exceeds stored bytes"
    );
    Ok(payload
        .chunks(2)
        .map(|chunk| u16::from_be_bytes([chunk[0], *chunk.get(1).unwrap_or(&0)]))
        .collect())
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
) -> Result<Vec<u8>> {
    let Some(Some(image_track)) = tracks.get_mut(track) else {
        bail!("target track {track} is empty");
    };
    match image_track {
        FloppyTrackImage::AmigaDos(track_data) => {
            let sectors = decode_non_empty_track_write(track, write_words)?;
            apply_amigados_track_sectors(track_data, track, &sectors)?;
            if legacy_extended_adf {
                encode_uae_legacy_extended_adf(tracks)
            } else {
                encode_uae_extended_adf(tracks)
            }
        }
        FloppyTrackImage::RawMfm {
            words,
            bit_len,
            stored_len,
            revolutions,
            legacy_sync,
            bitcell_ns,
        } => {
            *bitcell_ns = None;
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
                encode_uae_legacy_extended_adf(tracks)
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
                encode_uae_extended_adf(tracks)
            }
        }
    }
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
}

#[derive(serde::Serialize, serde::Deserialize)]
struct DiskDirectWrite {
    drive: usize,
    track: usize,
    write_words: Vec<u16>,
    write_start_word: usize,
    write_start_bit: u8,
}

#[derive(Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct DiskBytePos {
    drive: usize,
    track: usize,
    word: usize,
    byte_phase: u8,
}

#[derive(Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct DiskWordPos {
    drive: usize,
    track: usize,
    word: usize,
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
// bit phase so the post-sync word stream matches the disk's framing.
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

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::{write::GzEncoder, Compression};
    use std::fs;
    use std::io::Write;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn tick_index_flag_sync(ctrl: &mut FloppyController) {
        ctrl.tick(INDEX_FLAG_SYNC_CCK, 0, &mut []);
    }

    fn clear_index_flag(ctrl: &mut FloppyController) {
        if ctrl.index_flag_sync_cck != 0 {
            tick_index_flag_sync(ctrl);
        }
        ctrl.take_index_pulse();
    }

    fn bytes_to_words(bytes: &[u8]) -> Vec<u16> {
        bytes
            .chunks_exact(2)
            .map(|chunk| u16::from_be_bytes([chunk[0], chunk[1]]))
            .collect()
    }

    fn drive_select_prb(idx: usize, motor_on: bool) -> u8 {
        let mut prb = !CIAB_DSKSEL_MASKS[idx];
        if motor_on {
            prb &= !CIAB_DSKMOTOR;
        }
        prb
    }

    fn drive_deselect_prb(motor_on: bool) -> u8 {
        if motor_on {
            !CIAB_DSKMOTOR
        } else {
            0xFF
        }
    }

    fn read_external_drive_id(ctrl: &mut FloppyController, idx: usize) -> u32 {
        ctrl.write_prb(drive_deselect_prb(true));
        ctrl.write_prb(drive_select_prb(idx, true));
        ctrl.write_prb(drive_deselect_prb(true));
        ctrl.write_prb(drive_select_prb(idx, false));
        ctrl.write_prb(drive_deselect_prb(false));

        let mut id = 0u32;
        for _ in 0..32 {
            ctrl.write_prb(drive_select_prb(idx, false));
            id = (id << 1) | u32::from(ctrl.cia_a_status_bits() & CIAA_DSKRDY == 0);
            ctrl.write_prb(drive_deselect_prb(false));
        }
        id
    }

    #[test]
    fn standard_adf_geometry_matches_expected_size() {
        assert_eq!(ADF_SIZE, 901_120);
        assert_eq!(adf_sector_offset(159, 10) + BYTES_PER_SECTOR, ADF_SIZE);
    }

    #[test]
    fn mfm_encode_decode_round_trip_sector_data() -> Result<()> {
        let mut adf = vec![0u8; ADF_SIZE];
        for (i, b) in adf.iter_mut().take(BYTES_PER_SECTOR).enumerate() {
            *b = i as u8;
        }
        let words = encode_adf_track(0, &adf);
        let decoded = decode_track_write(0, &words)?;
        let sector0 = decoded.iter().find(|(sector, _)| *sector == 0).unwrap();
        assert_eq!(&sector0.1[..], &adf[0..BYTES_PER_SECTOR]);
        assert_eq!(decoded.len(), SECTORS_PER_TRACK);
        Ok(())
    }

    #[test]
    fn adz_floppy_decompresses_standard_adf_as_read_only() -> Result<()> {
        let mut adf = vec![0u8; ADF_SIZE];
        for (idx, byte) in adf.iter_mut().take(BYTES_PER_SECTOR).enumerate() {
            *byte = idx as u8;
        }
        let path = temp_adz(&adf)?;
        let cfg = FloppyConfig {
            drives: [
                Some(FloppyDriveConfig {
                    path: path.clone(),
                    write_protected: false,
                }),
                None,
                None,
                None,
            ],
        };

        let ctrl = FloppyController::from_config(&cfg)?;
        let image = ctrl.drives[0].image.as_ref().unwrap();
        assert!(image.write_protected);
        match &image.data {
            FloppyImageData::StandardAdf(decoded) => assert_eq!(decoded, &adf),
            FloppyImageData::Tracks(_) => panic!("ADZ should decode to a standard ADF image"),
        }

        let _ = fs::remove_file(path);
        Ok(())
    }

    #[test]
    fn gzip_floppy_can_wrap_uae_extended_adf_as_read_only() -> Result<()> {
        let raw_words = [0x4489, 0x2AAA, 0x5555, 0xA144];
        let ext_path = temp_ext2_raw(&raw_words)?;
        let ext_image = fs::read(&ext_path)?;
        let path = temp_gzip("test.ext.adf.gz", &ext_image)?;
        let cfg = FloppyConfig {
            drives: [
                Some(FloppyDriveConfig {
                    path: path.clone(),
                    write_protected: false,
                }),
                None,
                None,
                None,
            ],
        };
        let mut ctrl = FloppyController::from_config(&cfg)?;

        ctrl.write_prb(!CIAB_DSKMOTOR & !CIAB_DSKSEL0);
        ctrl.tick(MOTOR_READY_CCK, 0, &mut []);
        ctrl.ensure_track(0, 0);

        assert_eq!(ctrl.drives[0].cached_words(), raw_words);
        assert!(ctrl.drives[0]
            .image
            .as_ref()
            .is_some_and(|image| image.write_protected));

        let _ = fs::remove_file(ext_path);
        let _ = fs::remove_file(path);
        Ok(())
    }

    #[test]
    #[ignore = "IPF/CAPS support is an explicit non-goal until a direct parser or optional external library strategy is chosen"]
    fn ipf_caps_images_fail_with_explicit_strategy() -> Result<()> {
        let path = temp_path("test.ipf");
        let mut image = Vec::new();
        image.extend_from_slice(IPF_SIGNATURE);
        image.extend_from_slice(&12u32.to_be_bytes());
        image.extend_from_slice(&0u32.to_be_bytes());
        fs::write(&path, image)?;
        let cfg = FloppyConfig {
            drives: [
                Some(FloppyDriveConfig {
                    path: path.clone(),
                    write_protected: true,
                }),
                None,
                None,
                None,
            ],
        };

        let err = FloppyController::from_config(&cfg)
            .err()
            .expect("IPF/CAPS images should fail with the recorded strategy");
        let message = format!("{err:#}");
        assert!(message.contains("IPF/CAPS floppy image"));
        assert!(message.contains("explicit non-goal"));
        assert!(message.contains("SPS/CAPS library integration"));

        let _ = fs::remove_file(path);
        Ok(())
    }

    #[test]
    fn inserted_disk_image_asserts_change_and_preserves_drive_mechanics() -> Result<()> {
        let first = temp_adf()?;
        let second = temp_adf()?;
        let cfg = FloppyConfig {
            drives: [
                Some(FloppyDriveConfig {
                    path: first.clone(),
                    write_protected: true,
                }),
                None,
                None,
                None,
            ],
        };
        let mut ctrl = FloppyController::from_config(&cfg)?;
        ctrl.write_prb(!CIAB_DSKMOTOR & !CIAB_DSKSEL0);
        ctrl.tick(MOTOR_READY_CCK, 0, &mut []);
        ctrl.drives[0].step(true);
        let cylinder = ctrl.drives[0].cylinder;
        let motor_cck = ctrl.drives[0].motor_cck;

        ctrl.insert_disk_image(0, second.clone(), true)?;

        let drive = &ctrl.drives[0];
        assert_eq!(drive.cylinder, cylinder);
        assert_eq!(drive.motor_cck, motor_cck);
        assert!(drive.motor_on);
        assert!(drive.disk_change);
        assert_eq!(
            drive.image.as_ref().map(|image| image.path.as_path()),
            Some(second.as_path())
        );

        let _ = fs::remove_file(first);
        let _ = fs::remove_file(second);
        Ok(())
    }

    #[test]
    fn disk_change_line_settles_after_step_insert_and_eject() -> Result<()> {
        let first = temp_adf()?;
        let second = temp_adf()?;
        let cfg = FloppyConfig {
            drives: [
                Some(FloppyDriveConfig {
                    path: first.clone(),
                    write_protected: true,
                }),
                None,
                None,
                None,
            ],
        };
        let mut ctrl = FloppyController::from_config(&cfg)?;
        let selected = !CIAB_DSKSEL0;
        let inward_high = selected & !CIAB_DSKDIREC;
        ctrl.write_prb(inward_high);
        assert_eq!(ctrl.cia_a_status_bits() & CIAA_DSKCHANGE, 0);

        ctrl.write_prb(inward_high & !CIAB_DSKSTEP);
        assert_eq!(ctrl.cia_a_status_bits() & CIAA_DSKCHANGE, 0);
        ctrl.tick(DISK_STATUS_SETTLE_CCK, 0, &mut []);
        assert_ne!(ctrl.cia_a_status_bits() & CIAA_DSKCHANGE, 0);

        ctrl.insert_disk_image(0, second.clone(), true)?;
        assert_ne!(ctrl.cia_a_status_bits() & CIAA_DSKCHANGE, 0);
        ctrl.tick(DISK_STATUS_SETTLE_CCK, 0, &mut []);
        assert_eq!(ctrl.cia_a_status_bits() & CIAA_DSKCHANGE, 0);

        ctrl.write_prb(inward_high);
        ctrl.write_prb(inward_high & !CIAB_DSKSTEP);
        ctrl.tick(DISK_STATUS_SETTLE_CCK, 0, &mut []);
        assert_ne!(ctrl.cia_a_status_bits() & CIAA_DSKCHANGE, 0);

        ctrl.eject_disk_image(0)?;
        assert_ne!(ctrl.cia_a_status_bits() & CIAA_DSKCHANGE, 0);
        ctrl.tick(DISK_STATUS_SETTLE_CCK, 0, &mut []);
        assert_eq!(ctrl.cia_a_status_bits() & CIAA_DSKCHANGE, 0);

        let _ = fs::remove_file(first);
        let _ = fs::remove_file(second);
        Ok(())
    }

    #[test]
    fn write_protect_line_settles_after_inserted_disk_change() -> Result<()> {
        let protected = temp_adf()?;
        let writable = temp_adf()?;
        let cfg = FloppyConfig {
            drives: [
                Some(FloppyDriveConfig {
                    path: protected.clone(),
                    write_protected: true,
                }),
                None,
                None,
                None,
            ],
        };
        let mut ctrl = FloppyController::from_config(&cfg)?;
        ctrl.write_prb(!CIAB_DSKSEL0);
        assert_eq!(ctrl.cia_a_status_bits() & CIAA_DSKPROT, 0);

        ctrl.insert_disk_image(0, writable.clone(), false)?;
        assert_eq!(ctrl.cia_a_status_bits() & CIAA_DSKPROT, 0);
        ctrl.tick(DISK_STATUS_SETTLE_CCK, 0, &mut []);
        assert_ne!(ctrl.cia_a_status_bits() & CIAA_DSKPROT, 0);

        ctrl.insert_disk_image(0, protected.clone(), true)?;
        assert_ne!(ctrl.cia_a_status_bits() & CIAA_DSKPROT, 0);
        ctrl.tick(DISK_STATUS_SETTLE_CCK, 0, &mut []);
        assert_eq!(ctrl.cia_a_status_bits() & CIAA_DSKPROT, 0);

        let _ = fs::remove_file(protected);
        let _ = fs::remove_file(writable);
        Ok(())
    }

    #[test]
    fn cia_status_reflects_write_protect_track0_and_ready() -> Result<()> {
        let path = temp_adf()?;
        let cfg = FloppyConfig {
            drives: [
                Some(FloppyDriveConfig {
                    path: path.clone(),
                    write_protected: true,
                }),
                None,
                None,
                None,
            ],
        };
        let mut ctrl = FloppyController::from_config(&cfg)?;
        ctrl.write_prb(!CIAB_DSKMOTOR & !CIAB_DSKSEL0 & !CIAB_DSKSIDE);
        assert_eq!(ctrl.cia_a_status_bits() & CIAA_DSKPROT, 0);
        assert_eq!(ctrl.cia_a_status_bits() & CIAA_DSKTRACK0, 0);
        assert_ne!(ctrl.cia_a_status_bits() & CIAA_DSKRDY, 0);
        ctrl.tick(MOTOR_READY_CCK, 0, &mut []);
        assert_eq!(ctrl.cia_a_status_bits() & CIAA_DSKRDY, 0);
        let _ = fs::remove_file(path);
        Ok(())
    }

    #[test]
    fn cia_status_ready_line_tracks_motor_spinup_and_off() -> Result<()> {
        let path = temp_adf()?;
        let cfg = FloppyConfig {
            drives: [
                Some(FloppyDriveConfig {
                    path: path.clone(),
                    write_protected: true,
                }),
                None,
                None,
                None,
            ],
        };
        let mut ctrl = FloppyController::from_config(&cfg)?;
        let selected_motor_on = !CIAB_DSKMOTOR & !CIAB_DSKSEL0;
        let selected_motor_off = !CIAB_DSKSEL0;

        ctrl.write_prb(selected_motor_on);
        assert_ne!(ctrl.cia_a_status_bits() & CIAA_DSKRDY, 0);
        ctrl.tick(MOTOR_READY_CCK - 1, 0, &mut []);
        assert_ne!(ctrl.cia_a_status_bits() & CIAA_DSKRDY, 0);
        ctrl.tick(1, 0, &mut []);
        assert_eq!(ctrl.cia_a_status_bits() & CIAA_DSKRDY, 0);

        // While the motor line is off the ready line drops immediately...
        ctrl.write_prb(0xFF);
        ctrl.write_prb(selected_motor_off);
        assert_ne!(ctrl.cia_a_status_bits() & CIAA_DSKRDY, 0);
        // ...but the platter keeps spinning (inertia), so a brief off/on
        // toggle with no elapsed time re-asserts ready without a respin.
        ctrl.write_prb(0xFF);
        ctrl.write_prb(selected_motor_on);
        assert_eq!(ctrl.cia_a_status_bits() & CIAA_DSKRDY, 0);

        // A sustained motor-off spins the platter down: once enough time has
        // elapsed the drive is no longer ready and must spin up again.
        ctrl.write_prb(0xFF);
        ctrl.write_prb(selected_motor_off);
        ctrl.tick(MOTOR_READY_CCK, 0, &mut []);
        ctrl.write_prb(0xFF);
        ctrl.write_prb(selected_motor_on);
        assert_ne!(ctrl.cia_a_status_bits() & CIAA_DSKRDY, 0);
        ctrl.tick(MOTOR_READY_CCK, 0, &mut []);
        assert_eq!(ctrl.cia_a_status_bits() & CIAA_DSKRDY, 0);

        let _ = fs::remove_file(path);
        Ok(())
    }

    #[test]
    fn activity_led_follows_selected_drive_and_motor_line() {
        let mut ctrl = FloppyController::default();

        ctrl.write_prb(0xFF);
        assert!(!ctrl.activity_led_on());

        ctrl.write_prb(!CIAB_DSKSEL0);
        assert!(!ctrl.activity_led_on());

        ctrl.write_prb(0xFF);
        ctrl.write_prb(!CIAB_DSKSEL0 & !CIAB_DSKMOTOR);
        assert!(ctrl.activity_led_on());
    }

    #[test]
    fn side_select_maps_lower_head_to_even_adf_tracks() -> Result<()> {
        let path = temp_adf()?;
        let cfg = FloppyConfig {
            drives: [
                Some(FloppyDriveConfig {
                    path: path.clone(),
                    write_protected: true,
                }),
                None,
                None,
                None,
            ],
        };
        let mut ctrl = FloppyController::from_config(&cfg)?;

        ctrl.write_prb(!CIAB_DSKMOTOR & !CIAB_DSKSEL0);
        assert_eq!(ctrl.track_for_drive(0), 0);
        assert_eq!(ctrl.selected_track(), Some(0));

        ctrl.write_prb(!CIAB_DSKMOTOR & !CIAB_DSKSEL0 & !CIAB_DSKSIDE);
        assert_eq!(ctrl.track_for_drive(0), 1);
        assert_eq!(ctrl.selected_track(), Some(1));

        let _ = fs::remove_file(path);
        Ok(())
    }

    #[test]
    fn selected_track_follows_step_and_side_select_lines() {
        let mut ctrl = FloppyController::default();
        let lower_head = !CIAB_DSKMOTOR & !CIAB_DSKSEL0;
        ctrl.write_prb(lower_head);
        assert_eq!(ctrl.selected_track(), Some(0));

        let inward_step_high = lower_head & !CIAB_DSKDIREC;
        ctrl.write_prb(inward_step_high);
        ctrl.write_prb(inward_step_high & !CIAB_DSKSTEP);
        assert_eq!(ctrl.selected_track(), Some(2));

        ctrl.write_prb(inward_step_high & !CIAB_DSKSTEP & !CIAB_DSKSIDE);
        assert_eq!(ctrl.selected_track(), Some(3));

        ctrl.write_prb(0xFF);
        assert_eq!(ctrl.selected_track(), None);
    }

    #[test]
    fn step_pulses_move_head_on_each_falling_edge() {
        let mut ctrl = FloppyController::default();
        let lower_head = !CIAB_DSKMOTOR & !CIAB_DSKSEL0;
        let inward_high = lower_head & !CIAB_DSKDIREC;
        ctrl.write_prb(inward_high);

        ctrl.write_prb(inward_high & !CIAB_DSKSTEP);
        assert_eq!(ctrl.selected_track(), Some(2));

        ctrl.write_prb(inward_high);
        ctrl.write_prb(inward_high & !CIAB_DSKSTEP);
        assert_eq!(ctrl.selected_track(), Some(4));
    }

    #[test]
    fn step_direction_reversal_moves_on_next_falling_edge() {
        let mut ctrl = FloppyController::default();
        let lower_head = !CIAB_DSKMOTOR & !CIAB_DSKSEL0;
        let inward_high = lower_head & !CIAB_DSKDIREC;
        let outward_high = lower_head | CIAB_DSKDIREC;
        ctrl.write_prb(inward_high);

        ctrl.write_prb(inward_high & !CIAB_DSKSTEP);
        assert_eq!(ctrl.selected_track(), Some(2));

        ctrl.write_prb(outward_high);
        ctrl.write_prb(outward_high & !CIAB_DSKSTEP);
        assert_eq!(ctrl.selected_track(), Some(0));
    }

    #[test]
    fn track_zero_line_follows_head_position() -> Result<()> {
        let path = temp_adf()?;
        let cfg = FloppyConfig {
            drives: [
                Some(FloppyDriveConfig {
                    path: path.clone(),
                    write_protected: true,
                }),
                None,
                None,
                None,
            ],
        };
        let mut ctrl = FloppyController::from_config(&cfg)?;
        let selected = !CIAB_DSKSEL0;
        let inward_high = selected & !CIAB_DSKDIREC;
        let outward_high = selected | CIAB_DSKDIREC;
        // Power-on at cylinder 0: /TRK0 asserted (active-low, bit clear).
        ctrl.write_prb(inward_high);
        assert_eq!(ctrl.cia_a_status_bits() & CIAA_DSKTRACK0, 0);

        // One inward step to cylinder 1 de-asserts /TRK0 immediately, with no
        // settle delay (the position sensor follows the head, not the data
        // settle).
        ctrl.write_prb(inward_high & !CIAB_DSKSTEP);
        assert_eq!(ctrl.selected_track(), Some(2));
        assert_ne!(ctrl.cia_a_status_bits() & CIAA_DSKTRACK0, 0);

        // Stepping back out to cylinder 0 re-asserts /TRK0 immediately.
        ctrl.write_prb(outward_high);
        ctrl.write_prb(outward_high & !CIAB_DSKSTEP);
        assert_eq!(ctrl.selected_track(), Some(0));
        assert_eq!(ctrl.cia_a_status_bits() & CIAA_DSKTRACK0, 0);

        let _ = fs::remove_file(path);
        Ok(())
    }

    #[test]
    fn track_zero_asserts_immediately_during_rapid_recalibrate() {
        // A trackloader recalibrates by pulsing STEP outward and polling /TRK0
        // between pulses with no settle wait. /TRK0 must assert the moment the
        // head reaches cylinder 0, otherwise the loader steps past the stop or
        // hangs (the Magic Pockets recalibrate failure).
        let mut ctrl = FloppyController::default();
        let selected = !CIAB_DSKMOTOR & !CIAB_DSKSEL0;
        let inward_high = selected & !CIAB_DSKDIREC;
        let outward_high = selected | CIAB_DSKDIREC;

        // Seek inward 3 cylinders.
        for _ in 0..3 {
            ctrl.write_prb(inward_high);
            ctrl.write_prb(inward_high & !CIAB_DSKSTEP);
        }
        assert_eq!(ctrl.selected_track(), Some(6));
        assert_ne!(ctrl.cia_a_status_bits() & CIAA_DSKTRACK0, 0);

        // Rapid outward recalibrate; check /TRK0 after each step with no tick.
        ctrl.write_prb(outward_high);
        let mut asserted_at = None;
        for step in 1..=8 {
            ctrl.write_prb(outward_high & !CIAB_DSKSTEP);
            ctrl.write_prb(outward_high);
            if ctrl.cia_a_status_bits() & CIAA_DSKTRACK0 == 0 {
                asserted_at = Some(step);
                break;
            }
        }
        assert_eq!(asserted_at, Some(3));
        assert_eq!(ctrl.selected_track(), Some(0));
    }

    #[test]
    fn side_select_crosses_cylinder_head_boundaries_after_steps() {
        let mut ctrl = FloppyController::default();
        let lower_head = !CIAB_DSKMOTOR & !CIAB_DSKSEL0;
        let inward_high = lower_head & !CIAB_DSKDIREC;
        let outward_high = lower_head | CIAB_DSKDIREC;

        ctrl.write_prb(inward_high);
        ctrl.write_prb(inward_high & !CIAB_DSKSTEP);
        assert_eq!(ctrl.selected_track(), Some(2));
        ctrl.write_prb((inward_high & !CIAB_DSKSTEP) & !CIAB_DSKSIDE);
        assert_eq!(ctrl.selected_track(), Some(3));

        ctrl.write_prb(outward_high & !CIAB_DSKSIDE);
        ctrl.write_prb((outward_high & !CIAB_DSKSIDE) & !CIAB_DSKSTEP);
        assert_eq!(ctrl.selected_track(), Some(1));
        ctrl.write_prb(outward_high);
        assert_eq!(ctrl.selected_track(), Some(0));
    }

    #[test]
    fn cia_b_drive_select_lines_map_df0_bit3_to_df3_bit6() {
        let mut ctrl = FloppyController::default();

        for (idx, select_mask) in CIAB_DSKSEL_MASKS.iter().enumerate() {
            ctrl.write_prb(!select_mask);
            assert_eq!(ctrl.selected_drive(), Some(idx));
        }

        ctrl.write_prb(0xFF);
        assert_eq!(ctrl.selected_drive(), None);
    }

    #[test]
    fn external_drive_id_reads_shift_msb_first_on_rdy() {
        let mut ctrl = FloppyController::default();
        ctrl.drives[1].external_id = 0xA5A5_0001;

        assert_eq!(read_external_drive_id(&mut ctrl, 1), 0xA5A5_0001);
        assert_eq!(ctrl.drives[1].external_id_bit, 32);
        assert!(!ctrl.drives[1].motor_on);
    }

    #[test]
    fn external_drive_selects_follow_daisy_chain_order_for_df1_to_df3() {
        let mut ctrl = FloppyController::default();
        let ids = [0, 0x8000_0001, 0x4000_0002, 0x2000_0003];
        for idx in 1..=3 {
            ctrl.drives[idx].external_id = ids[idx];
        }

        for idx in 1..=3 {
            assert_eq!(read_external_drive_id(&mut ctrl, idx), ids[idx]);
            ctrl.write_prb(drive_select_prb(idx, false));
            assert_eq!(ctrl.selected_drive(), Some(idx));
            ctrl.write_prb(drive_deselect_prb(false));
        }
    }

    #[test]
    fn unconfigured_external_drive_slots_do_not_answer_drive_id() {
        let mut ctrl = FloppyController::default();

        for idx in 1..ctrl.drives.len() {
            assert_eq!(read_external_drive_id(&mut ctrl, idx), 0);
        }
    }

    #[test]
    fn configured_external_drive_defaults_to_standard_amiga_drive_id() -> Result<()> {
        let path = temp_adf()?;
        let mut ctrl = FloppyController::default();
        ctrl.drives[1] = FloppyDrive::load(&FloppyDriveConfig {
            path,
            write_protected: true,
        })?;

        assert_eq!(
            read_external_drive_id(&mut ctrl, 1),
            STANDARD_EXTERNAL_DRIVE_ID
        );
        Ok(())
    }

    #[test]
    fn connected_empty_external_drive_answers_standard_drive_id() {
        let mut ctrl = FloppyController::default();
        ctrl.set_connected_drives([true, true, false, true]);

        assert!(ctrl.drive_connected(1));
        assert!(!ctrl.disk_inserted(1));
        assert_eq!(
            read_external_drive_id(&mut ctrl, 1),
            STANDARD_EXTERNAL_DRIVE_ID
        );
        assert!(!ctrl.drive_connected(2));
        assert_eq!(read_external_drive_id(&mut ctrl, 2), 0);
        assert!(ctrl.drive_connected(3));
        assert_eq!(
            read_external_drive_id(&mut ctrl, 3),
            STANDARD_EXTERNAL_DRIVE_ID
        );
    }

    #[test]
    fn internal_df0_motor_follows_selected_motor_line_level() {
        let mut ctrl = FloppyController::default();

        ctrl.write_prb(drive_select_prb(0, true));
        assert!(ctrl.drives[0].motor_on);
        ctrl.write_prb(drive_select_prb(0, false));
        assert!(!ctrl.drives[0].motor_on);
        ctrl.write_prb(drive_select_prb(0, true));
        assert!(ctrl.drives[0].motor_on);
    }

    #[test]
    fn external_drive_mtrxd_latches_only_on_select_active_edge() {
        let mut ctrl = FloppyController::default();
        let idx = 1;

        ctrl.write_prb(drive_select_prb(idx, false));
        assert!(!ctrl.drives[idx].motor_on);
        ctrl.write_prb(drive_select_prb(idx, true));
        assert!(!ctrl.drives[idx].motor_on);

        ctrl.write_prb(drive_deselect_prb(true));
        ctrl.write_prb(drive_select_prb(idx, true));
        assert!(ctrl.drives[idx].motor_on);
        ctrl.write_prb(drive_select_prb(idx, false));
        assert!(ctrl.drives[idx].motor_on);

        ctrl.write_prb(drive_deselect_prb(false));
        ctrl.write_prb(drive_select_prb(idx, false));
        assert!(!ctrl.drives[idx].motor_on);
    }

    #[test]
    fn dresb_does_not_reset_internal_df0_motor_latch() {
        let mut ctrl = FloppyController::default();

        ctrl.write_prb(drive_select_prb(0, true));
        ctrl.tick(MOTOR_READY_CCK, 0, &mut []);
        assert!(ctrl.drives[0].motor_on);

        ctrl.reset_external_drives();

        assert!(ctrl.drives[0].motor_on);
    }

    #[test]
    fn dresb_resets_external_motor_latch_and_write_protect_sense() {
        let mut ctrl = FloppyController::default();
        let idx = 1;

        ctrl.drives[idx].write_protected_target = false;
        ctrl.drives[idx].write_protected_sense = false;
        ctrl.write_prb(drive_select_prb(idx, true));
        ctrl.tick(MOTOR_READY_CCK, 0, &mut []);
        assert!(ctrl.drives[idx].motor_on);
        assert!(!ctrl.drives[idx].write_protected_sense);

        ctrl.reset_external_drives();
        assert!(!ctrl.drives[idx].motor_on);
        assert_eq!(ctrl.drives[idx].motor_cck, 0);
        assert!(ctrl.drives[idx].write_protected_sense);

        ctrl.write_prb(drive_select_prb(idx, false));
        assert_eq!(ctrl.cia_a_status_bits() & CIAA_DSKPROT, 0);
        ctrl.tick(DISK_STATUS_SETTLE_CCK, 0, &mut []);
        assert_ne!(ctrl.cia_a_status_bits() & CIAA_DSKPROT, 0);
    }

    #[test]
    fn dskbytr_byte_valid_tracks_new_rotation_words() -> Result<()> {
        let path = temp_adf()?;
        let cfg = FloppyConfig {
            drives: [
                Some(FloppyDriveConfig {
                    path: path.clone(),
                    write_protected: true,
                }),
                None,
                None,
                None,
            ],
        };
        let mut ctrl = FloppyController::from_config(&cfg)?;
        ctrl.write_prb(!CIAB_DSKMOTOR & !CIAB_DSKSEL0);
        ctrl.tick(MOTOR_READY_CCK, 0, &mut []);

        ctrl.ensure_track(0, 0);
        let first_word = ctrl.drives[0].cached_words()[ctrl.drives[0].rotation_word_index()];
        ctrl.write_dsksync(first_word);

        let first = ctrl.read_dskbytr(0, 0);
        assert_ne!(first & DSKBYT, 0);
        assert_ne!(first & WORDEQUAL, 0);

        let second = ctrl.read_dskbytr(0, 0);
        assert_eq!(second & DSKBYT, 0);
        assert_ne!(second & WORDEQUAL, 0);

        ctrl.tick(ctrl.word_cck(), 0, &mut []);
        let third = ctrl.read_dskbytr(0, 0);
        assert_ne!(third & DSKBYT, 0);

        let _ = fs::remove_file(path);
        Ok(())
    }

    #[test]
    fn dskbytr_reports_current_disk_byte_phase() -> Result<()> {
        let raw_words = [0x1234, 0xABCD];
        let path = temp_ext2_raw(&raw_words)?;
        let cfg = FloppyConfig {
            drives: [
                Some(FloppyDriveConfig {
                    path: path.clone(),
                    write_protected: true,
                }),
                None,
                None,
                None,
            ],
        };
        let mut ctrl = FloppyController::from_config(&cfg)?;

        ctrl.write_prb(!CIAB_DSKMOTOR & !CIAB_DSKSEL0);
        ctrl.tick(MOTOR_READY_CCK, 0, &mut []);
        ctrl.ensure_track(0, 0);
        ctrl.drives[0].set_rotation_word(0);
        ctrl.drives[0].rotation_acc_cck = 0;
        let word_cck = FloppyController::word_cck_for_track_words(raw_words.len());
        let half_word_cck = word_cck.div_ceil(2);

        let high = ctrl.read_dskbytr(0, 0);
        assert_ne!(high & DSKBYT, 0);
        assert_eq!(high & 0x00FF, 0x12);

        let repeat_high = ctrl.read_dskbytr(0, 0);
        assert_eq!(repeat_high & DSKBYT, 0);
        assert_eq!(repeat_high & 0x00FF, 0x12);

        ctrl.tick(half_word_cck, 0, &mut []);
        let low = ctrl.read_dskbytr(0, 0);
        assert_ne!(low & DSKBYT, 0);
        assert_eq!(low & 0x00FF, 0x34);

        ctrl.tick(word_cck - half_word_cck, 0, &mut []);
        let next_high = ctrl.read_dskbytr(0, 0);
        assert_ne!(next_high & DSKBYT, 0);
        assert_eq!(next_high & 0x00FF, 0xAB);

        let _ = fs::remove_file(path);
        Ok(())
    }

    #[test]
    fn bit_stream_assembles_aligned_disk_bytes() {
        let words = [0x1234, 0xABCD];

        let high = DiskBitStream::from_word_phase(&words, words.len(), 0, 0).unwrap();
        assert_eq!(high.bit_position(), 0);
        assert_eq!(high.index_position(), 0);
        assert_eq!(high.storage_word_position(), 0);
        assert_eq!(high.storage_word(), 0x1234);
        assert_eq!(high.assembled_byte(), 0x12);

        let low = DiskBitStream::from_word_phase(&words, words.len(), 0, 8).unwrap();
        assert_eq!(low.bit_position(), 8);
        assert_eq!(low.index_position(), 8);
        assert_eq!(low.storage_word_position(), 0);
        assert_eq!(low.storage_word(), 0x1234);
        assert_eq!(low.assembled_byte(), 0x34);
    }

    #[test]
    fn bit_stream_assembles_sub_word_disk_bytes() {
        let words = [0x1234, 0xABCD];

        let mid_word = DiskBitStream::from_word_phase(&words, words.len(), 0, 4).unwrap();
        assert_eq!(mid_word.bit_position(), 4);
        assert_eq!(mid_word.assembled_byte(), 0x23);

        let cross_word = DiskBitStream::from_word_phase(&words, words.len(), 0, 12).unwrap();
        assert_eq!(cross_word.bit_position(), 12);
        assert_eq!(cross_word.assembled_byte(), 0x4A);
    }

    #[test]
    fn bit_stream_reports_index_relative_position() {
        let words = [0x1111, 0x2222, 0x3333, 0x4444];
        let stream = DiskBitStream::from_word_phase(&words, 2, 3, 5).unwrap();

        assert_eq!(stream.bit_position(), 53);
        assert_eq!(stream.index_position(), 21);
        assert_eq!(stream.storage_word_position(), 3);
        assert_eq!(stream.storage_word(), 0x4444);
    }

    #[test]
    fn bit_stream_detects_sync_words_at_bit_phase() {
        let words = [0x1234, 0xABCD];

        let aligned = DiskBitStream::from_word_phase(&words, words.len(), 0, 0).unwrap();
        assert!(aligned.sync_matches(0x1234));
        assert!(!aligned.sync_matches(0x234A));

        let cross_word = DiskBitStream::from_word_phase(&words, words.len(), 0, 12).unwrap();
        assert!(cross_word.sync_matches(0x4ABC));
        assert_eq!(cross_word.assembled_word(), 0x4ABC);
    }

    #[test]
    fn bit_stream_uses_rotation_bit_phase() {
        let words = [0x1234, 0xF0AA];
        let word_cck = 160;
        let stream =
            DiskBitStream::from_rotation(&words, words.len(), 1, word_cck / 4, word_cck).unwrap();

        assert_eq!(stream.bit_position(), 20);
        assert_eq!(stream.storage_word_position(), 1);
        assert_eq!(stream.assembled_byte(), 0x0A);
    }

    #[test]
    fn dpll_fifo_shifts_disk_bytes_and_read_words() {
        let words = [0x1234, 0xABCD];
        let stream = DiskBitStream::from_word_phase(&words, words.len(), 0, 0).unwrap();
        let mut dpll = PaulaDiskReadDpllFifo::new();

        dpll.sample_stream_range(&stream, 0, 8, DEFAULT_DSKSYNC);
        let first_byte = dpll.read_dskbytr();
        assert_ne!(first_byte & DSKBYT, 0);
        assert_eq!(first_byte & 0x00FF, 0x12);
        assert_eq!(dpll.fifo_len(), 0);
        assert_eq!(dpll.read_dskbytr() & DSKBYT, 0);

        dpll.sample_stream_range(&stream, 8, 8, DEFAULT_DSKSYNC);
        let second_byte = dpll.read_dskbytr();
        assert_ne!(second_byte & DSKBYT, 0);
        assert_eq!(second_byte & 0x00FF, 0x34);
        assert_eq!(dpll.fifo_len(), 1);
        assert_eq!(dpll.read_fifo_word(), Some(0x1234));

        dpll.sample_stream_range(&stream, 16, 16, DEFAULT_DSKSYNC);
        assert_eq!(dpll.fifo_len(), 1);
        assert_eq!(dpll.read_fifo_word(), Some(0xABCD));
        assert_eq!(dpll.read_fifo_word(), None);
    }

    #[test]
    fn dpll_fifo_detects_unaligned_disk_sync_word() {
        let words = [0x1234, 0xABCD];
        let stream = DiskBitStream::from_word_phase(&words, words.len(), 0, 4).unwrap();
        let mut dpll = PaulaDiskReadDpllFifo::new();

        dpll.sample_stream_range(&stream, 0, 8, 0x234A);
        let first_byte = dpll.read_dskbytr();
        assert_ne!(first_byte & DSKBYT, 0);
        assert_eq!(first_byte & 0x00FF, 0x23);
        assert_eq!(first_byte & WORDEQUAL, 0);
        assert!(!dpll.take_sync_irq());

        dpll.sample_stream_range(&stream, 8, 8, 0x234A);
        let second_byte = dpll.read_dskbytr();
        assert_ne!(second_byte & DSKBYT, 0);
        assert_eq!(second_byte & 0x00FF, 0x4A);
        assert_ne!(second_byte & WORDEQUAL, 0);
        assert!(dpll.take_sync_irq());
        assert_eq!(dpll.read_fifo_word(), Some(0x234A));

        dpll.sample_stream_range(&stream, 16, 1, 0x234A);
        assert_eq!(dpll.read_dskbytr() & WORDEQUAL, 0);
    }

    #[test]
    fn dpll_fifo_preserves_oldest_words_when_full() {
        let words = [0x1111, 0x2222, 0x3333, 0x4444];
        let stream = DiskBitStream::from_word_phase(&words, words.len(), 0, 0).unwrap();
        let mut dpll = PaulaDiskReadDpllFifo::new();

        dpll.sample_stream_bits(&stream, words.len() * 16, DEFAULT_DSKSYNC);

        assert_eq!(dpll.fifo_len(), 3);
        assert!(dpll.fifo_overflowed());
        assert_eq!(dpll.read_fifo_word(), Some(0x1111));
        assert_eq!(dpll.read_fifo_word(), Some(0x2222));
        assert_eq!(dpll.read_fifo_word(), Some(0x3333));
        assert_eq!(dpll.read_fifo_word(), None);
    }

    #[test]
    fn dpll_fifo_dskbytr_read_clears_byte_ready_only() {
        let words = [DEFAULT_DSKSYNC, 0x2222];
        let stream = DiskBitStream::from_word_phase(&words, words.len(), 0, 0).unwrap();
        let mut dpll = PaulaDiskReadDpllFifo::new();

        dpll.sample_stream_bits(&stream, 16, DEFAULT_DSKSYNC);

        let first = dpll.read_dskbytr();
        assert_ne!(first & DSKBYT, 0);
        assert_ne!(first & WORDEQUAL, 0);
        assert_eq!(first & 0x00FF, 0x89);
        assert!(dpll.take_sync_irq());

        let repeat = dpll.read_dskbytr();
        assert_eq!(repeat & DSKBYT, 0);
        assert_ne!(repeat & WORDEQUAL, 0);
        assert_eq!(repeat & 0x00FF, 0x89);
    }

    #[test]
    fn dskbytr_wordequal_tracks_current_word() -> Result<()> {
        let raw_words = [0x1234, 0x5678];
        let path = temp_ext2_raw(&raw_words)?;
        let cfg = FloppyConfig {
            drives: [
                Some(FloppyDriveConfig {
                    path: path.clone(),
                    write_protected: true,
                }),
                None,
                None,
                None,
            ],
        };
        let mut ctrl = FloppyController::from_config(&cfg)?;

        ctrl.write_prb(!CIAB_DSKMOTOR & !CIAB_DSKSEL0);
        ctrl.tick(MOTOR_READY_CCK, 0, &mut []);
        ctrl.ensure_track(0, 0);
        ctrl.drives[0].set_rotation_word(0);
        ctrl.drives[0].rotation_acc_cck = 0;
        ctrl.write_dsksync(raw_words[0]);

        let first = ctrl.read_dskbytr(0, 0);
        assert_ne!(first & WORDEQUAL, 0);
        let repeat = ctrl.read_dskbytr(0, 0);
        assert_ne!(repeat & WORDEQUAL, 0);

        ctrl.tick(
            FloppyController::word_cck_for_track_words(raw_words.len()),
            0,
            &mut [],
        );
        let next = ctrl.read_dskbytr(0, 0);
        assert_eq!(next & WORDEQUAL, 0);

        let _ = fs::remove_file(path);
        Ok(())
    }

    #[test]
    fn msbsync_dskbytr_keeps_wordequal_without_stream_irq() -> Result<()> {
        let raw_words = [DEFAULT_DSKSYNC, 0x5678];
        let path = temp_ext2_raw(&raw_words)?;
        let cfg = FloppyConfig {
            drives: [
                Some(FloppyDriveConfig {
                    path: path.clone(),
                    write_protected: true,
                }),
                None,
                None,
                None,
            ],
        };
        let mut ctrl = FloppyController::from_config(&cfg)?;

        ctrl.write_prb(!CIAB_DSKMOTOR & !CIAB_DSKSEL0);
        ctrl.tick(MOTOR_READY_CCK, 0, &mut []);
        ctrl.ensure_track(0, 0);
        ctrl.drives[0].set_rotation_word(0);
        ctrl.drives[0].rotation_acc_cck = 0;

        let first = ctrl.read_dskbytr(0, ADK_MSBSYNC);
        assert_ne!(first & DSKBYT, 0);
        assert_ne!(first & WORDEQUAL, 0);
        assert!(!ctrl.take_sync_irq());

        let _ = fs::remove_file(path);
        Ok(())
    }

    #[test]
    fn write_dma_dskbytr_keeps_wordequal_without_stream_irq() -> Result<()> {
        let raw_words = [DEFAULT_DSKSYNC, 0x5678];
        let path = temp_ext2_raw(&raw_words)?;
        let cfg = FloppyConfig {
            drives: [
                Some(FloppyDriveConfig {
                    path: path.clone(),
                    write_protected: true,
                }),
                None,
                None,
                None,
            ],
        };
        let mut ctrl = FloppyController::from_config(&cfg)?;

        ctrl.write_prb(!CIAB_DSKMOTOR & !CIAB_DSKSEL0);
        ctrl.tick(MOTOR_READY_CCK, 0, &mut []);
        ctrl.ensure_track(0, 0);
        ctrl.drives[0].set_rotation_word(0);
        ctrl.drives[0].rotation_acc_cck = 0;

        let len = DSKLEN_DMAEN | DSKLEN_WRITE | 1;
        assert!(!ctrl.write_dsklen(len, 0));
        assert!(!ctrl.write_dsklen(len, 0));

        let status = ctrl.read_dskbytr(DMACON_DMAEN | DMACON_DISK, 0);
        assert_ne!(status & DSKBYT, 0);
        assert_ne!(status & DMAON, 0);
        assert_ne!(status & DISKWRITE, 0);
        assert_ne!(status & WORDEQUAL, 0);
        assert_eq!(status & 0x00FF, 0x00);
        assert!(!ctrl.take_sync_irq());

        let _ = fs::remove_file(path);
        Ok(())
    }

    #[test]
    fn dskbytr_write_mode_without_dma_suppresses_byte_loads() -> Result<()> {
        let raw_words = [0x1234, DEFAULT_DSKSYNC];
        let path = temp_ext2_raw(&raw_words)?;
        let cfg = FloppyConfig {
            drives: [
                Some(FloppyDriveConfig {
                    path: path.clone(),
                    write_protected: true,
                }),
                None,
                None,
                None,
            ],
        };
        let mut ctrl = FloppyController::from_config(&cfg)?;

        ctrl.write_prb(!CIAB_DSKMOTOR & !CIAB_DSKSEL0);
        ctrl.tick(MOTOR_READY_CCK, 0, &mut []);
        ctrl.ensure_track(0, 0);
        ctrl.drives[0].set_rotation_word(0);
        ctrl.drives[0].rotation_acc_cck = 0;

        let first = ctrl.read_dskbytr(0, 0);
        assert_ne!(first & DSKBYT, 0);
        assert_eq!(first & 0x00FF, 0x12);

        assert!(!ctrl.write_dsksync(DEFAULT_DSKSYNC));
        assert!(!ctrl.write_dsklen(DSKLEN_WRITE, 0));
        let word_cck = FloppyController::word_cck_for_track_words(raw_words.len());
        ctrl.tick(word_cck, 0, &mut []);

        let write_mode = ctrl.read_dskbytr(0, 0);
        assert_ne!(write_mode & DISKWRITE, 0);
        assert_eq!(write_mode & DSKBYT, 0);
        assert_ne!(write_mode & WORDEQUAL, 0);
        assert_eq!(write_mode & 0x00FF, 0x12);
        assert!(ctrl.take_sync_irq());

        assert!(!ctrl.write_dsklen(0, 0));
        let resumed = ctrl.read_dskbytr(0, 0);
        assert_ne!(resumed & DSKBYT, 0);
        assert_eq!(resumed & 0x00FF, 0x44);

        let _ = fs::remove_file(path);
        Ok(())
    }

    #[test]
    fn dskbytr_dmaon_waits_for_double_dsklen_arm() -> Result<()> {
        let raw_words = [0x1234, 0x5678];
        let path = temp_ext2_raw(&raw_words)?;
        let cfg = FloppyConfig {
            drives: [
                Some(FloppyDriveConfig {
                    path: path.clone(),
                    write_protected: true,
                }),
                None,
                None,
                None,
            ],
        };
        let mut ctrl = FloppyController::from_config(&cfg)?;

        ctrl.write_prb(!CIAB_DSKMOTOR & !CIAB_DSKSEL0);
        ctrl.tick(MOTOR_READY_CCK, 0, &mut []);

        let len = DSKLEN_DMAEN | 1;
        let dmacon = DMACON_DMAEN | DMACON_DISK;
        assert!(!ctrl.write_dsklen(len, 0));
        assert_eq!(ctrl.read_dskbytr(dmacon, 0) & DMAON, 0);

        assert!(!ctrl.write_dsklen(len, 0));
        assert_ne!(ctrl.read_dskbytr(dmacon, 0) & DMAON, 0);
        assert_eq!(ctrl.read_dskbytr(0, 0) & DMAON, 0);

        let _ = fs::remove_file(path);
        Ok(())
    }

    #[test]
    fn dskbytr_dmaon_stays_clear_when_armed_dma_cannot_start() -> Result<()> {
        let raw_words = [0x1234, 0x5678];
        let path = temp_ext2_raw(&raw_words)?;
        let cfg = FloppyConfig {
            drives: [
                Some(FloppyDriveConfig {
                    path: path.clone(),
                    write_protected: true,
                }),
                None,
                None,
                None,
            ],
        };
        let mut ctrl = FloppyController::from_config(&cfg)?;

        ctrl.write_prb(!CIAB_DSKMOTOR & !CIAB_DSKSEL0);

        let len = DSKLEN_DMAEN | 1;
        let dmacon = DMACON_DMAEN | DMACON_DISK;
        assert!(!ctrl.write_dsklen(len, 0));
        assert!(ctrl.write_dsklen(len, 0));
        assert_eq!(ctrl.read_dskbytr(dmacon, 0) & DMAON, 0);

        let _ = fs::remove_file(path);
        Ok(())
    }

    #[test]
    fn read_dma_before_motor_ready_does_not_start() -> Result<()> {
        let raw_words = [0x1234, 0x5678];
        let path = temp_ext2_raw(&raw_words)?;
        let cfg = FloppyConfig {
            drives: [
                Some(FloppyDriveConfig {
                    path: path.clone(),
                    write_protected: true,
                }),
                None,
                None,
                None,
            ],
        };
        let mut ctrl = FloppyController::from_config(&cfg)?;
        let mut chip_ram = vec![0u8; 4];

        ctrl.write_prb(!CIAB_DSKMOTOR & !CIAB_DSKSEL0);
        ctrl.set_dskpt_low(0);
        let len = DSKLEN_DMAEN | 1;
        assert!(!ctrl.write_dsklen(len, 0));
        assert!(ctrl.write_dsklen(len, 0));
        let dmacon = DMACON_DMAEN | DMACON_DISK;

        assert_eq!(ctrl.read_dskbytr(dmacon, 0) & DMAON, 0);
        assert_eq!(ctrl.next_completion_cck(dmacon), None);
        assert!(!ctrl.tick(
            FloppyController::word_cck_for_track_words(raw_words.len()),
            dmacon,
            &mut chip_ram
        ));
        assert_eq!(read_chip_word(&chip_ram, 0), 0);

        let _ = fs::remove_file(path);
        Ok(())
    }

    #[test]
    fn motor_off_blocks_read_dma_until_next_spinup() -> Result<()> {
        let raw_words = [0x1234, 0x5678];
        let path = temp_ext2_raw(&raw_words)?;
        let cfg = FloppyConfig {
            drives: [
                Some(FloppyDriveConfig {
                    path: path.clone(),
                    write_protected: true,
                }),
                None,
                None,
                None,
            ],
        };
        let mut ctrl = FloppyController::from_config(&cfg)?;
        let mut chip_ram = vec![0u8; 4];
        let selected_motor_on = !CIAB_DSKMOTOR & !CIAB_DSKSEL0;
        let selected_motor_off = !CIAB_DSKSEL0;

        ctrl.write_prb(selected_motor_on);
        ctrl.tick(MOTOR_READY_CCK, 0, &mut chip_ram);
        assert_eq!(ctrl.cia_a_status_bits() & CIAA_DSKRDY, 0);
        ctrl.write_prb(0xFF);
        ctrl.write_prb(selected_motor_off);
        assert_ne!(ctrl.cia_a_status_bits() & CIAA_DSKRDY, 0);

        ctrl.set_dskpt_low(0);
        let len = DSKLEN_DMAEN | 1;
        assert!(!ctrl.write_dsklen(len, 0));
        assert!(ctrl.write_dsklen(len, 0));
        let dmacon = DMACON_DMAEN | DMACON_DISK;
        assert_eq!(ctrl.read_dskbytr(dmacon, 0) & DMAON, 0);

        // Leave the motor off long enough for the platter to spin down fully,
        // so the drive must spin back up before the next read can run.
        ctrl.tick(MOTOR_READY_CCK, 0, &mut chip_ram);
        ctrl.write_prb(0xFF);
        ctrl.write_prb(selected_motor_on);
        assert_ne!(ctrl.cia_a_status_bits() & CIAA_DSKRDY, 0);
        ctrl.tick(MOTOR_READY_CCK, 0, &mut chip_ram);
        assert_eq!(ctrl.cia_a_status_bits() & CIAA_DSKRDY, 0);

        let len = DSKLEN_DMAEN | 1;
        assert!(!ctrl.write_dsklen(len, 0));
        assert!(!ctrl.write_dsklen(len, 0));
        assert_ne!(ctrl.read_dskbytr(dmacon, 0) & DMAON, 0);

        let _ = fs::remove_file(path);
        Ok(())
    }

    #[test]
    fn dsksync_write_latches_current_word_match() -> Result<()> {
        let path = temp_adf()?;
        let cfg = FloppyConfig {
            drives: [
                Some(FloppyDriveConfig {
                    path: path.clone(),
                    write_protected: true,
                }),
                None,
                None,
                None,
            ],
        };
        let mut ctrl = FloppyController::from_config(&cfg)?;
        ctrl.write_prb(!CIAB_DSKMOTOR & !CIAB_DSKSEL0);
        ctrl.tick(MOTOR_READY_CCK, 0, &mut []);
        ctrl.ensure_track(0, 0);
        let current_word = ctrl.drives[0].cached_words()[ctrl.drives[0].rotation_word_index()];

        assert!(ctrl.write_dsksync(current_word));
        assert!(ctrl.take_sync_irq());

        let _ = fs::remove_file(path);
        Ok(())
    }

    #[test]
    fn reset_dsksync_defaults_to_amigados_mfm_sync_word() -> Result<()> {
        let raw_words = [DEFAULT_DSKSYNC, 0x2222];
        let path = temp_ext2_raw(&raw_words)?;
        let cfg = FloppyConfig {
            drives: [
                Some(FloppyDriveConfig {
                    path: path.clone(),
                    write_protected: true,
                }),
                None,
                None,
                None,
            ],
        };
        let mut ctrl = FloppyController::from_config(&cfg)?;

        ctrl.write_prb(!CIAB_DSKMOTOR & !CIAB_DSKSEL0);
        ctrl.tick(MOTOR_READY_CCK, 0, &mut []);
        ctrl.ensure_track(0, 0);
        ctrl.drives[0].set_rotation_word(0);
        ctrl.drives[0].rotation_acc_cck = 0;

        let status = ctrl.read_dskbytr(0, 0);
        assert_ne!(status & WORDEQUAL, 0);
        assert!(ctrl.take_sync_irq());

        let _ = fs::remove_file(path);
        Ok(())
    }

    #[test]
    fn read_dma_sync_irq_does_not_require_wordsync() -> Result<()> {
        let path = temp_adf()?;
        let cfg = FloppyConfig {
            drives: [
                Some(FloppyDriveConfig {
                    path: path.clone(),
                    write_protected: true,
                }),
                None,
                None,
                None,
            ],
        };
        let mut ctrl = FloppyController::from_config(&cfg)?;
        let mut chip_ram = vec![0u8; 8];
        ctrl.write_prb(!CIAB_DSKMOTOR & !CIAB_DSKSEL0);
        ctrl.tick(MOTOR_READY_CCK, 0, &mut chip_ram);
        ctrl.ensure_track(0, 0);
        let sync_pos = ctrl.drives[0]
            .cached_words()
            .iter()
            .position(|&word| word == DEFAULT_DSKSYNC)
            .unwrap();
        ctrl.drives[0].set_rotation_word(sync_pos);
        assert!(ctrl.write_dsksync(DEFAULT_DSKSYNC));
        assert!(ctrl.take_sync_irq());

        ctrl.set_dskpt_low(0);
        let len = DSKLEN_DMAEN | 1;
        assert!(!ctrl.write_dsklen(len, 0));
        assert!(!ctrl.write_dsklen(len, 0));
        assert!(ctrl.tick(ctrl.word_cck(), DMACON_DMAEN | DMACON_DISK, &mut chip_ram));

        assert!(ctrl.take_sync_irq());
        assert_eq!(read_chip_word(&chip_ram, 0), DEFAULT_DSKSYNC);

        let _ = fs::remove_file(path);
        Ok(())
    }

    #[test]
    fn wordsync_skips_initial_sync_then_transfers_repeated_sync_word() -> Result<()> {
        let path = temp_adf()?;
        let cfg = FloppyConfig {
            drives: [
                Some(FloppyDriveConfig {
                    path: path.clone(),
                    write_protected: true,
                }),
                None,
                None,
                None,
            ],
        };
        let mut ctrl = FloppyController::from_config(&cfg)?;
        let mut chip_ram = vec![0u8; 8];
        ctrl.write_prb(!CIAB_DSKMOTOR & !CIAB_DSKSEL0);
        ctrl.tick(MOTOR_READY_CCK, 0, &mut chip_ram);
        assert!(!ctrl.write_dsksync(DEFAULT_DSKSYNC));
        ctrl.ensure_track(0, 0);
        let sync_pos = ctrl.drives[0]
            .cached_words()
            .iter()
            .position(|&word| word == DEFAULT_DSKSYNC)
            .unwrap();
        ctrl.drives[0].set_rotation_word(sync_pos);

        ctrl.set_dskpt_low(0);
        let len = DSKLEN_DMAEN | 1;
        assert!(!ctrl.write_dsklen(len, ADK_WORDSYNC));
        assert!(!ctrl.write_dsklen(len, ADK_WORDSYNC));
        let dmacon = DMACON_DMAEN | DMACON_DISK;

        assert!(!ctrl.tick(ctrl.word_cck(), dmacon, &mut chip_ram));
        assert_eq!(read_chip_word(&chip_ram, 0), 0);

        assert!(ctrl.take_sync_irq());
        assert!(ctrl.tick(ctrl.word_cck(), dmacon, &mut chip_ram));
        assert_eq!(read_chip_word(&chip_ram, 0), DEFAULT_DSKSYNC);

        let _ = fs::remove_file(path);
        Ok(())
    }

    #[test]
    fn wordsync_locks_onto_bit_aligned_sync_and_frames_following_word() -> Result<()> {
        // 0x4489 straddles a word boundary, occupying bits 8..24: word0's low
        // byte is 0x44 and word1's high byte is 0x89. A word-grid scan never
        // sees it; the bit-level shifter locks on at bit 23 and frames the
        // first transferred word from bit 24 (word1 low byte 0x55 + word2 high
        // byte 0x12 = 0x5512).
        let raw_words = [0xAA44u16, 0x8955, 0x1234, 0x5678, 0x0000];
        let path = temp_ext2_raw(&raw_words)?;
        let cfg = FloppyConfig {
            drives: [
                Some(FloppyDriveConfig {
                    path: path.clone(),
                    write_protected: true,
                }),
                None,
                None,
                None,
            ],
        };
        let mut ctrl = FloppyController::from_config(&cfg)?;
        let mut chip_ram = vec![0u8; 8];
        ctrl.write_prb(!CIAB_DSKMOTOR & !CIAB_DSKSEL0);
        ctrl.tick(MOTOR_READY_CCK, 0, &mut chip_ram);
        ctrl.ensure_track(0, 0);
        ctrl.drives[0].set_rotation_bit(0);
        ctrl.drives[0].rotation_acc_cck = 0;

        ctrl.set_dskpt_low(0);
        let len = DSKLEN_DMAEN | 1;
        assert!(!ctrl.write_dsklen(len, ADK_WORDSYNC));
        assert!(!ctrl.write_dsklen(len, ADK_WORDSYNC));
        let dmacon = DMACON_DMAEN | DMACON_DISK;
        let word_cck = FloppyController::word_cck_for_track_words(raw_words.len());

        let mut done = false;
        for _ in 0..5 {
            if ctrl.tick(word_cck, dmacon, &mut chip_ram) {
                done = true;
                break;
            }
        }
        assert!(done, "bit-aligned sync-wait DMA should complete");
        assert!(ctrl.take_sync_irq());
        assert_eq!(read_chip_word(&chip_ram, 0), 0x5512);

        let _ = fs::remove_file(path);
        Ok(())
    }

    #[test]
    fn read_dma_sync_irq_deadline_tracks_next_sync_word() -> Result<()> {
        let raw_words = [0x1111, DEFAULT_DSKSYNC, 0x2222];
        let path = temp_ext2_raw(&raw_words)?;
        let cfg = FloppyConfig {
            drives: [
                Some(FloppyDriveConfig {
                    path: path.clone(),
                    write_protected: true,
                }),
                None,
                None,
                None,
            ],
        };
        let mut ctrl = FloppyController::from_config(&cfg)?;
        let mut chip_ram = vec![0u8; 8];
        ctrl.write_prb(!CIAB_DSKMOTOR & !CIAB_DSKSEL0);
        ctrl.tick(MOTOR_READY_CCK, 0, &mut chip_ram);
        ctrl.ensure_track(0, 0);
        ctrl.drives[0].set_rotation_word(0);
        ctrl.drives[0].rotation_acc_cck = 0;

        ctrl.set_dskpt_low(0);
        let len = DSKLEN_DMAEN | raw_words.len() as u16;
        assert!(!ctrl.write_dsklen(len, 0));
        assert!(!ctrl.write_dsklen(len, 0));
        let dmacon = DMACON_DMAEN | DMACON_DISK;
        let word_cck = FloppyController::word_cck_for_track_words(raw_words.len());

        assert_eq!(ctrl.next_completion_cck(dmacon), Some(3 * word_cck));
        assert_eq!(ctrl.next_sync_irq_cck(dmacon), Some(2 * word_cck));

        assert!(!ctrl.tick(word_cck - 1, dmacon, &mut chip_ram));
        assert_eq!(ctrl.next_sync_irq_cck(dmacon), Some(word_cck + 1));
        assert!(!ctrl.take_sync_irq());

        assert!(!ctrl.tick(1, dmacon, &mut chip_ram));
        assert_eq!(ctrl.next_sync_irq_cck(dmacon), Some(word_cck));
        assert!(!ctrl.take_sync_irq());

        assert!(!ctrl.tick(word_cck, dmacon, &mut chip_ram));
        assert!(ctrl.take_sync_irq());
        assert_eq!(read_chip_word(&chip_ram, 2), DEFAULT_DSKSYNC);

        let _ = fs::remove_file(path);
        Ok(())
    }

    #[test]
    fn active_read_dma_dsksync_change_updates_dskbytr_wordequal() -> Result<()> {
        let raw_words = [0x1111, 0x2222, 0x3333];
        let path = temp_ext2_raw(&raw_words)?;
        let cfg = FloppyConfig {
            drives: [
                Some(FloppyDriveConfig {
                    path: path.clone(),
                    write_protected: true,
                }),
                None,
                None,
                None,
            ],
        };
        let mut ctrl = FloppyController::from_config(&cfg)?;
        let mut chip_ram = vec![0u8; 8];
        ctrl.write_prb(!CIAB_DSKMOTOR & !CIAB_DSKSEL0);
        ctrl.tick(MOTOR_READY_CCK, 0, &mut chip_ram);
        ctrl.ensure_track(0, 0);
        ctrl.drives[0].set_rotation_word(0);
        ctrl.drives[0].rotation_acc_cck = 0;
        assert!(!ctrl.write_dsksync(0xFFFF));

        ctrl.set_dskpt_low(0);
        let len = DSKLEN_DMAEN | raw_words.len() as u16;
        assert!(!ctrl.write_dsklen(len, 0));
        assert!(!ctrl.write_dsklen(len, 0));
        let dmacon = DMACON_DMAEN | DMACON_DISK;
        let word_cck = FloppyController::word_cck_for_track_words(raw_words.len());

        assert!(!ctrl.tick(word_cck, dmacon, &mut chip_ram));
        assert_eq!(read_chip_word(&chip_ram, 0), raw_words[0]);
        assert!(!ctrl.take_sync_irq());

        assert!(ctrl.write_dsksync(raw_words[1]));
        let status = ctrl.read_dskbytr(dmacon, 0);
        assert_ne!(status & DMAON, 0);
        assert_ne!(status & WORDEQUAL, 0);
        assert!(ctrl.take_sync_irq());

        let _ = fs::remove_file(path);
        Ok(())
    }

    #[test]
    fn active_read_dma_dsklen_rewrite_updates_remaining_length() -> Result<()> {
        let raw_words = [0x1111, 0x2222, 0x3333, 0x4444];
        let path = temp_ext2_raw(&raw_words)?;
        let cfg = FloppyConfig {
            drives: [
                Some(FloppyDriveConfig {
                    path: path.clone(),
                    write_protected: true,
                }),
                None,
                None,
                None,
            ],
        };
        let mut ctrl = FloppyController::from_config(&cfg)?;
        let mut chip_ram = vec![0u8; 8];
        ctrl.write_prb(!CIAB_DSKMOTOR & !CIAB_DSKSEL0);
        ctrl.tick(MOTOR_READY_CCK, 0, &mut chip_ram);
        ctrl.ensure_track(0, 0);
        ctrl.drives[0].set_rotation_word(0);
        ctrl.drives[0].rotation_acc_cck = 0;

        ctrl.set_dskpt_low(0);
        let len = DSKLEN_DMAEN | raw_words.len() as u16;
        assert!(!ctrl.write_dsklen(len, 0));
        assert!(!ctrl.write_dsklen(len, 0));
        let dmacon = DMACON_DMAEN | DMACON_DISK;
        let word_cck = FloppyController::word_cck_for_track_words(raw_words.len());

        assert!(!ctrl.tick(word_cck, dmacon, &mut chip_ram));
        assert_eq!(read_chip_word(&chip_ram, 0), 0x1111);

        assert!(!ctrl.write_dsklen(DSKLEN_DMAEN | 1, 0));
        assert_eq!(ctrl.next_completion_cck(dmacon), Some(word_cck));
        assert!(ctrl.tick(word_cck, dmacon, &mut chip_ram));

        assert_eq!(read_chip_word(&chip_ram, 0), 0x1111);
        assert_eq!(read_chip_word(&chip_ram, 2), 0x2222);
        assert_eq!(read_chip_word(&chip_ram, 4), 0x0000);

        let _ = fs::remove_file(path);
        Ok(())
    }

    #[test]
    fn active_read_dma_dsklen_zero_rewrite_finishes_now() -> Result<()> {
        let raw_words = [0x1111, 0x2222, 0x3333, 0x4444];
        let path = temp_ext2_raw(&raw_words)?;
        let cfg = FloppyConfig {
            drives: [
                Some(FloppyDriveConfig {
                    path: path.clone(),
                    write_protected: true,
                }),
                None,
                None,
                None,
            ],
        };
        let mut ctrl = FloppyController::from_config(&cfg)?;
        let mut chip_ram = vec![0u8; 8];
        ctrl.write_prb(!CIAB_DSKMOTOR & !CIAB_DSKSEL0);
        ctrl.tick(MOTOR_READY_CCK, 0, &mut chip_ram);
        ctrl.ensure_track(0, 0);
        ctrl.drives[0].set_rotation_word(0);
        ctrl.drives[0].rotation_acc_cck = 0;

        ctrl.set_dskpt_low(0);
        let len = DSKLEN_DMAEN | 2;
        assert!(!ctrl.write_dsklen(len, 0));
        assert!(!ctrl.write_dsklen(len, 0));
        let dmacon = DMACON_DMAEN | DMACON_DISK;
        let word_cck = FloppyController::word_cck_for_track_words(raw_words.len());

        assert!(ctrl.write_dsklen(DSKLEN_DMAEN, 0));
        assert_eq!(ctrl.next_completion_cck(dmacon), None);
        assert!(!ctrl.tick(word_cck, dmacon, &mut chip_ram));
        assert_eq!(read_chip_word(&chip_ram, 0), 0x0000);

        let _ = fs::remove_file(path);
        Ok(())
    }

    #[test]
    fn active_read_dma_follows_head_step_to_new_track() -> Result<()> {
        let track0 = [0x1111, 0x2222];
        let track2 = [0xAAAA, 0xBBBB];
        let path = temp_ext2_raw_tracks(&[(0, &track0), (2, &track2)])?;
        let cfg = FloppyConfig {
            drives: [
                Some(FloppyDriveConfig {
                    path: path.clone(),
                    write_protected: true,
                }),
                None,
                None,
                None,
            ],
        };
        let mut ctrl = FloppyController::from_config(&cfg)?;
        let mut chip_ram = vec![0u8; 8];

        let selected = !CIAB_DSKMOTOR & !CIAB_DSKSEL0;
        ctrl.write_prb(selected);
        ctrl.tick(MOTOR_READY_CCK, 0, &mut chip_ram);
        ctrl.ensure_track(0, 0);
        ctrl.drives[0].set_rotation_word(0);
        ctrl.drives[0].rotation_acc_cck = 0;
        ctrl.set_dskpt_low(0);
        let len = DSKLEN_DMAEN | 3;
        assert!(!ctrl.write_dsklen(len, 0));
        assert!(!ctrl.write_dsklen(len, 0));
        let dmacon = DMACON_DMAEN | DMACON_DISK;
        let word_cck = FloppyController::word_cck_for_track_words(track0.len());

        assert!(!ctrl.tick(word_cck, dmacon, &mut chip_ram));
        assert_eq!(read_chip_word(&chip_ram, 0), 0x1111);

        let step_high = selected & !CIAB_DSKDIREC;
        ctrl.write_prb(step_high);
        ctrl.write_prb(step_high & !CIAB_DSKSTEP);
        assert_eq!(ctrl.track_for_drive(0), 2);

        assert!(!ctrl.tick(word_cck, dmacon, &mut chip_ram));
        assert_eq!(read_chip_word(&chip_ram, 2), 0xBBBB);
        assert!(ctrl.tick(word_cck, dmacon, &mut chip_ram));
        assert_eq!(read_chip_word(&chip_ram, 4), 0xAAAA);

        let _ = fs::remove_file(path);
        Ok(())
    }

    // A read issued while the head is still settling after a step recovers no
    // data (the cells under the moving head are garbage), while the platter
    // keeps spinning -- so the read resumes a rotation-latency later, modelling
    // a real drive's post-seek settle. /TRK0 and the cylinder index stay instant.
    #[test]
    fn read_dma_suppressed_during_post_seek_settle() -> Result<()> {
        let track0 = [0x1111u16, 0x2222, 0x3333, 0x4444];
        let path = temp_ext2_raw_tracks(&[(0, &track0)])?;
        let cfg = FloppyConfig {
            drives: [
                Some(FloppyDriveConfig {
                    path: path.clone(),
                    write_protected: true,
                }),
                None,
                None,
                None,
            ],
        };
        let mut ctrl = FloppyController::from_config(&cfg)?;
        let mut chip_ram = vec![0u8; 16];

        let selected = !CIAB_DSKMOTOR & !CIAB_DSKSEL0;
        ctrl.write_prb(selected);
        ctrl.tick(MOTOR_READY_CCK, 0, &mut chip_ram);
        ctrl.ensure_track(0, 0);
        ctrl.drives[0].set_rotation_word(0);
        ctrl.drives[0].rotation_acc_cck = 0;
        ctrl.set_dskpt_low(0);
        let len = DSKLEN_DMAEN | 4;
        assert!(!ctrl.write_dsklen(len, 0));
        assert!(!ctrl.write_dsklen(len, 0));
        let dmacon = DMACON_DMAEN | DMACON_DISK;
        let word_cck = FloppyController::word_cck_for_track_words(track0.len());

        // First word reads normally.
        assert!(!ctrl.tick(word_cck, dmacon, &mut chip_ram));
        assert_eq!(read_chip_word(&chip_ram, 0), 0x1111);

        // Force a settle window spanning the next two ticks: while it is active
        // the DMA makes no progress even though the head keeps rotating.
        ctrl.drives[0].seek_settle_cck = word_cck * 3;
        assert!(!ctrl.tick(word_cck, dmacon, &mut chip_ram));
        assert_eq!(read_chip_word(&chip_ram, 2), 0x0000);
        assert!(!ctrl.tick(word_cck, dmacon, &mut chip_ram));
        assert_eq!(read_chip_word(&chip_ram, 2), 0x0000);

        // Settle elapsed: the read resumes and recovers a (rotated) track word.
        assert!(!ctrl.tick(word_cck, dmacon, &mut chip_ram));
        assert_ne!(read_chip_word(&chip_ram, 2), 0x0000);

        let _ = fs::remove_file(path);
        Ok(())
    }

    #[test]
    fn wordsync_read_dma_zero_rewrite_finishes_at_sync() -> Result<()> {
        let raw_words = [0x1111, DEFAULT_DSKSYNC, 0x2222];
        let path = temp_ext2_raw(&raw_words)?;
        let cfg = FloppyConfig {
            drives: [
                Some(FloppyDriveConfig {
                    path: path.clone(),
                    write_protected: true,
                }),
                None,
                None,
                None,
            ],
        };
        let mut ctrl = FloppyController::from_config(&cfg)?;
        let mut chip_ram = vec![0u8; 8];
        ctrl.write_prb(!CIAB_DSKMOTOR & !CIAB_DSKSEL0);
        ctrl.tick(MOTOR_READY_CCK, 0, &mut chip_ram);
        ctrl.ensure_track(0, 0);
        ctrl.drives[0].set_rotation_word(0);
        ctrl.drives[0].rotation_acc_cck = 0;

        ctrl.set_dskpt_low(0);
        let len = DSKLEN_DMAEN | 2;
        assert!(!ctrl.write_dsklen(len, ADK_WORDSYNC));
        assert!(!ctrl.write_dsklen(len, ADK_WORDSYNC));
        let dmacon = DMACON_DMAEN | DMACON_DISK;
        let word_cck = FloppyController::word_cck_for_track_words(raw_words.len());

        assert!(!ctrl.write_dsklen(DSKLEN_DMAEN, ADK_WORDSYNC));
        assert_eq!(ctrl.next_completion_cck(dmacon), None);
        assert!(!ctrl.tick(word_cck, dmacon, &mut chip_ram));
        assert_eq!(read_chip_word(&chip_ram, 0), 0x0000);
        assert!(ctrl.tick(word_cck, dmacon, &mut chip_ram));
        assert!(ctrl.take_sync_irq());
        assert_eq!(read_chip_word(&chip_ram, 0), 0x0000);

        let _ = fs::remove_file(path);
        Ok(())
    }

    #[test]
    fn wordsync_wait_reports_sync_deadline_without_completion_deadline() -> Result<()> {
        let raw_words = [0x1111, DEFAULT_DSKSYNC, 0x2222];
        let path = temp_ext2_raw(&raw_words)?;
        let cfg = FloppyConfig {
            drives: [
                Some(FloppyDriveConfig {
                    path: path.clone(),
                    write_protected: true,
                }),
                None,
                None,
                None,
            ],
        };
        let mut ctrl = FloppyController::from_config(&cfg)?;
        let mut chip_ram = vec![0u8; 8];
        ctrl.write_prb(!CIAB_DSKMOTOR & !CIAB_DSKSEL0);
        ctrl.tick(MOTOR_READY_CCK, 0, &mut chip_ram);
        ctrl.ensure_track(0, 0);
        ctrl.drives[0].set_rotation_word(0);
        ctrl.drives[0].rotation_acc_cck = 0;

        ctrl.set_dskpt_low(0);
        let len = DSKLEN_DMAEN | 1;
        assert!(!ctrl.write_dsklen(len, ADK_WORDSYNC));
        assert!(!ctrl.write_dsklen(len, ADK_WORDSYNC));
        let dmacon = DMACON_DMAEN | DMACON_DISK;
        let word_cck = FloppyController::word_cck_for_track_words(raw_words.len());

        assert_eq!(ctrl.next_completion_cck(dmacon), None);
        assert_eq!(ctrl.next_sync_irq_cck(dmacon), Some(2 * word_cck));

        let _ = fs::remove_file(path);
        Ok(())
    }

    #[test]
    fn msbsync_read_dma_suppresses_stream_sync_irq_deadline() -> Result<()> {
        let raw_words = [0x1111, DEFAULT_DSKSYNC, 0x2222];
        let path = temp_ext2_raw(&raw_words)?;
        let cfg = FloppyConfig {
            drives: [
                Some(FloppyDriveConfig {
                    path: path.clone(),
                    write_protected: true,
                }),
                None,
                None,
                None,
            ],
        };
        let mut ctrl = FloppyController::from_config(&cfg)?;
        let mut chip_ram = vec![0u8; 8];
        ctrl.write_prb(!CIAB_DSKMOTOR & !CIAB_DSKSEL0);
        ctrl.tick(MOTOR_READY_CCK, 0, &mut chip_ram);
        ctrl.ensure_track(0, 0);
        ctrl.drives[0].set_rotation_word(0);
        ctrl.drives[0].rotation_acc_cck = 0;

        ctrl.set_dskpt_low(0);
        let len = DSKLEN_DMAEN | raw_words.len() as u16;
        assert!(!ctrl.write_dsklen(len, ADK_MSBSYNC));
        assert!(!ctrl.write_dsklen(len, ADK_MSBSYNC));
        let dmacon = DMACON_DMAEN | DMACON_DISK;
        let word_cck = FloppyController::word_cck_for_track_words(raw_words.len());

        assert_eq!(ctrl.next_completion_cck(dmacon), Some(3 * word_cck));
        assert_eq!(ctrl.next_sync_irq_cck(dmacon), None);

        assert!(!ctrl.tick(word_cck, dmacon, &mut chip_ram));
        assert!(!ctrl.tick(word_cck, dmacon, &mut chip_ram));
        assert!(!ctrl.take_sync_irq());
        assert_eq!(read_chip_word(&chip_ram, 2), DEFAULT_DSKSYNC);
        assert!(ctrl.tick(word_cck, dmacon, &mut chip_ram));
        assert!(!ctrl.take_sync_irq());

        let _ = fs::remove_file(path);
        Ok(())
    }

    #[test]
    fn msbsync_wordsync_wait_ignores_dsksync_word_match() -> Result<()> {
        let raw_words = [DEFAULT_DSKSYNC, 0x2222];
        let path = temp_ext2_raw(&raw_words)?;
        let cfg = FloppyConfig {
            drives: [
                Some(FloppyDriveConfig {
                    path: path.clone(),
                    write_protected: true,
                }),
                None,
                None,
                None,
            ],
        };
        let mut ctrl = FloppyController::from_config(&cfg)?;
        let mut chip_ram = vec![0u8; 4];
        ctrl.write_prb(!CIAB_DSKMOTOR & !CIAB_DSKSEL0);
        ctrl.tick(MOTOR_READY_CCK, 0, &mut chip_ram);
        ctrl.ensure_track(0, 0);
        ctrl.drives[0].set_rotation_word(0);
        ctrl.drives[0].rotation_acc_cck = 0;

        ctrl.set_dskpt_low(0);
        let len = DSKLEN_DMAEN | 1;
        let adkcon = ADK_WORDSYNC | ADK_MSBSYNC;
        assert!(!ctrl.write_dsklen(len, adkcon));
        assert!(!ctrl.write_dsklen(len, adkcon));
        let dmacon = DMACON_DMAEN | DMACON_DISK;
        let word_cck = FloppyController::word_cck_for_track_words(raw_words.len());

        assert_eq!(ctrl.next_completion_cck(dmacon), None);
        assert_eq!(ctrl.next_sync_irq_cck(dmacon), None);
        assert!(!ctrl.tick(word_cck, dmacon, &mut chip_ram));
        assert!(!ctrl.take_sync_irq());
        assert_eq!(read_chip_word(&chip_ram, 0), 0);
        assert_eq!(ctrl.next_completion_cck(dmacon), None);

        let _ = fs::remove_file(path);
        Ok(())
    }

    #[test]
    fn read_dma_completion_deadline_preserves_sub_word_elapsed_time() -> Result<()> {
        let raw_words = [0x1111, 0x2222];
        let path = temp_ext2_raw(&raw_words)?;
        let cfg = FloppyConfig {
            drives: [
                Some(FloppyDriveConfig {
                    path: path.clone(),
                    write_protected: true,
                }),
                None,
                None,
                None,
            ],
        };
        let mut ctrl = FloppyController::from_config(&cfg)?;
        let mut chip_ram = vec![0u8; 4];
        ctrl.write_prb(!CIAB_DSKMOTOR & !CIAB_DSKSEL0);
        ctrl.tick(MOTOR_READY_CCK, 0, &mut chip_ram);
        ctrl.ensure_track(0, 0);
        ctrl.drives[0].set_rotation_word(0);
        ctrl.drives[0].rotation_acc_cck = 0;

        ctrl.set_dskpt_low(0);
        let len = DSKLEN_DMAEN | 1;
        assert!(!ctrl.write_dsklen(len, 0));
        assert!(!ctrl.write_dsklen(len, 0));
        let dmacon = DMACON_DMAEN | DMACON_DISK;
        let word_cck = FloppyController::word_cck_for_track_words(raw_words.len());

        assert!(!ctrl.tick(word_cck - 2, dmacon, &mut chip_ram));
        assert_eq!(ctrl.next_completion_cck(dmacon), Some(2));
        assert!(!ctrl.tick(1, dmacon, &mut chip_ram));
        assert_eq!(ctrl.next_completion_cck(dmacon), Some(1));
        assert!(ctrl.tick(1, dmacon, &mut chip_ram));

        let _ = fs::remove_file(path);
        Ok(())
    }

    #[test]
    fn selected_drive_index_pulse_latches_once_per_wrap() -> Result<()> {
        let path = temp_adf()?;
        let cfg = FloppyConfig {
            drives: [
                Some(FloppyDriveConfig {
                    path: path.clone(),
                    write_protected: true,
                }),
                None,
                None,
                None,
            ],
        };
        let mut ctrl = FloppyController::from_config(&cfg)?;
        ctrl.write_prb(!CIAB_DSKMOTOR & !CIAB_DSKSEL0);
        ctrl.tick(MOTOR_READY_CCK, 0, &mut []);
        clear_index_flag(&mut ctrl);

        ctrl.drives[0].set_rotation_word(encoded_track_words() - 1);
        ctrl.drives[0].rotation_acc_cck = 0;
        ctrl.tick(ctrl.word_cck(), 0, &mut []);
        assert!(ctrl.index_pulse_active());
        assert!(!ctrl.take_index_pulse());
        assert_eq!(ctrl.next_index_pulse_cck(), Some(INDEX_FLAG_SYNC_CCK));
        tick_index_flag_sync(&mut ctrl);
        assert!(ctrl.take_index_pulse());
        assert!(!ctrl.take_index_pulse());

        let _ = fs::remove_file(path);
        Ok(())
    }

    #[test]
    fn selected_drive_index_pulse_has_fixed_width() -> Result<()> {
        let path = temp_adf()?;
        let cfg = FloppyConfig {
            drives: [
                Some(FloppyDriveConfig {
                    path: path.clone(),
                    write_protected: true,
                }),
                None,
                None,
                None,
            ],
        };
        let mut ctrl = FloppyController::from_config(&cfg)?;
        ctrl.write_prb(!CIAB_DSKMOTOR & !CIAB_DSKSEL0);
        ctrl.tick(MOTOR_READY_CCK, 0, &mut []);
        clear_index_flag(&mut ctrl);

        ctrl.drives[0].set_rotation_word(encoded_track_words() - 1);
        ctrl.drives[0].rotation_acc_cck = 0;
        ctrl.tick(ctrl.word_cck(), 0, &mut []);

        assert!(ctrl.index_pulse_active());
        tick_index_flag_sync(&mut ctrl);
        assert!(ctrl.take_index_pulse());
        assert!(ctrl.index_pulse_active());

        ctrl.tick(INDEX_PULSE_CCK - INDEX_FLAG_SYNC_CCK - 1, 0, &mut []);
        assert!(ctrl.index_pulse_active());
        ctrl.tick(1, 0, &mut []);
        assert!(!ctrl.index_pulse_active());

        let _ = fs::remove_file(path);
        Ok(())
    }

    #[test]
    fn motor_off_drive_does_not_emit_index_pulse() -> Result<()> {
        let path = temp_adf()?;
        let cfg = FloppyConfig {
            drives: [
                Some(FloppyDriveConfig {
                    path: path.clone(),
                    write_protected: true,
                }),
                None,
                None,
                None,
            ],
        };
        let mut ctrl = FloppyController::from_config(&cfg)?;
        ctrl.write_prb(!CIAB_DSKSEL0);
        ctrl.ensure_track(0, 0);
        ctrl.drives[0].set_rotation_word(encoded_track_words() - 1);
        ctrl.drives[0].rotation_acc_cck = 0;

        assert_eq!(ctrl.next_index_pulse_cck(), None);
        ctrl.tick(ctrl.word_cck(), 0, &mut []);
        assert!(!ctrl.index_pulse_active());
        assert!(!ctrl.take_index_pulse());

        let _ = fs::remove_file(path);
        Ok(())
    }

    #[test]
    fn next_index_pulse_reports_selected_drive_time() -> Result<()> {
        let path = temp_adf()?;
        let cfg = FloppyConfig {
            drives: [
                Some(FloppyDriveConfig {
                    path: path.clone(),
                    write_protected: true,
                }),
                None,
                None,
                None,
            ],
        };
        let mut ctrl = FloppyController::from_config(&cfg)?;
        ctrl.write_prb(0xFF);
        assert_eq!(ctrl.next_index_pulse_cck(), None);

        ctrl.write_prb(!CIAB_DSKMOTOR & !CIAB_DSKSEL0);
        ctrl.drives[0].set_rotation_word(encoded_track_words() - 2);
        assert_eq!(ctrl.next_index_pulse_cck(), Some(ctrl.word_cck() * 2));

        ctrl.tick(ctrl.word_cck(), 0, &mut []);
        assert_eq!(ctrl.next_index_pulse_cck(), Some(ctrl.word_cck()));

        ctrl.drives[0].rotation_acc_cck = ctrl.word_cck() - 2;
        assert_eq!(ctrl.next_index_pulse_cck(), Some(2));

        let _ = fs::remove_file(path);
        Ok(())
    }

    #[test]
    fn uae_extended_adf_raw_track_exposes_mfm_words() -> Result<()> {
        let raw_words = [0x4489, 0x2AAA, 0x5555, 0xA144];
        let path = temp_ext2_raw(&raw_words)?;
        let cfg = FloppyConfig {
            drives: [
                Some(FloppyDriveConfig {
                    path: path.clone(),
                    write_protected: false,
                }),
                None,
                None,
                None,
            ],
        };
        let mut ctrl = FloppyController::from_config(&cfg)?;

        ctrl.write_prb(!CIAB_DSKMOTOR & !CIAB_DSKSEL0);
        ctrl.tick(MOTOR_READY_CCK, 0, &mut []);
        ctrl.ensure_track(0, 0);

        assert_eq!(ctrl.drives[0].cached_words(), raw_words);
        assert!(ctrl.drives[0]
            .image
            .as_ref()
            .is_some_and(|image| !image.write_protected));

        let _ = fs::remove_file(path);
        Ok(())
    }

    #[test]
    fn uae_extended_adf_raw_track_preserves_odd_byte_payload() -> Result<()> {
        let path = temp_ext2_track(1, 20, &[0x12, 0x34, 0xA0])?;
        let cfg = FloppyConfig {
            drives: [
                Some(FloppyDriveConfig {
                    path: path.clone(),
                    write_protected: false,
                }),
                None,
                None,
                None,
            ],
        };
        let mut ctrl = FloppyController::from_config(&cfg)?;
        let mut chip_ram = vec![0xFF, 0xFF];

        ctrl.write_prb(!CIAB_DSKMOTOR & !CIAB_DSKSEL0);
        ctrl.tick(MOTOR_READY_CCK, 0, &mut chip_ram);
        ctrl.ensure_track(0, 0);
        assert_eq!(ctrl.drives[0].cached_words(), [0x1234, 0xA000]);

        ctrl.drives[0].set_rotation_word(0);
        ctrl.drives[0].rotation_acc_cck = 0;
        ctrl.set_dskpt_low(0);
        let len = DSKLEN_DMAEN | DSKLEN_WRITE | 1;
        assert!(!ctrl.write_dsklen(len, 0));
        assert!(!ctrl.write_dsklen(len, 0));
        let dmacon = DMACON_DMAEN | DMACON_DISK;
        let word_cck = FloppyController::word_cck_for_track_words(2);
        assert!(ctrl.tick(word_cck, dmacon, &mut chip_ram));

        let persisted = fs::read(&path)?;
        let desc = &persisted[12..24];
        assert_eq!(
            u32::from_be_bytes([desc[4], desc[5], desc[6], desc[7]]) as usize,
            3
        );
        assert_eq!(
            u32::from_be_bytes([desc[8], desc[9], desc[10], desc[11]]),
            20
        );
        assert_eq!(&persisted[24..27], &[0xFF, 0xFC, 0xA0]);

        let mut reloaded = FloppyController::from_config(&cfg)?;
        reloaded.write_prb(!CIAB_DSKMOTOR & !CIAB_DSKSEL0);
        reloaded.tick(MOTOR_READY_CCK, 0, &mut []);
        reloaded.ensure_track(0, 0);
        assert_eq!(reloaded.drives[0].cached_words(), [0xFFFC, 0xA000]);

        let _ = fs::remove_file(path);
        Ok(())
    }

    #[test]
    fn raw_mfm_replacement_preserves_odd_stored_byte_capacity() -> Result<()> {
        let mut words = Vec::new();
        let mut bit_len = 0;
        let mut stored_len = 5;
        let mut revolutions = 3;

        apply_raw_mfm_write(
            &mut words,
            &mut bit_len,
            &mut stored_len,
            &mut revolutions,
            &[0xABCD],
            0,
            0,
            true,
        )?;

        assert_eq!(words, [0xABC8, 0x0000, 0x0000]);
        assert_eq!(bit_len, 16);
        assert_eq!(stored_len, 5);
        assert_eq!(revolutions, 1);
        assert_eq!(
            raw_words_payload(&words, (stored_len * 8) as u32, 0),
            [0xAB, 0xC8, 0x00, 0x00, 0x00]
        );
        Ok(())
    }

    #[test]
    fn uae_extended_adf_raw_track_cycles_stored_revolutions() -> Result<()> {
        let raw_words: [u16; 4] = [0x1111, 0x2222, 0x3333, 0x4444];
        let path = temp_ext2_raw_revolutions(&raw_words, 32, 2)?;
        let cfg = FloppyConfig {
            drives: [
                Some(FloppyDriveConfig {
                    path: path.clone(),
                    write_protected: true,
                }),
                None,
                None,
                None,
            ],
        };
        let mut ctrl = FloppyController::from_config(&cfg)?;

        ctrl.write_prb(!CIAB_DSKMOTOR & !CIAB_DSKSEL0);
        ctrl.tick(MOTOR_READY_CCK, 0, &mut []);
        clear_index_flag(&mut ctrl);
        ctrl.ensure_track(0, 0);
        ctrl.drives[0].set_rotation_word(0);
        ctrl.drives[0].rotation_acc_cck = 0;

        // Per-revolution: the two captured revolutions are stored separately,
        // each its exact 32-bit length (no concatenation seam).
        assert_eq!(ctrl.drives[0].cached.revs.len(), 2);
        assert_eq!(ctrl.drives[0].cached.revs[0].words, [0x1111, 0x2222]);
        assert_eq!(ctrl.drives[0].cached.revs[1].words, [0x3333, 0x4444]);
        assert_eq!(ctrl.drives[0].cached_index_words(), 2);
        let word_cck = FloppyController::word_cck_for_track_words(2);
        assert_eq!(ctrl.next_index_pulse_cck(), Some(word_cck * 2));
        assert_eq!(ctrl.next_disk_word(0, 0), Some(0x1111));
        assert_eq!(ctrl.next_disk_word(0, 0), Some(0x2222));
        assert!(!ctrl.take_index_pulse());
        tick_index_flag_sync(&mut ctrl);
        assert!(ctrl.take_index_pulse());
        assert_eq!(ctrl.next_disk_word(0, 0), Some(0x3333));
        assert_eq!(ctrl.next_disk_word(0, 0), Some(0x4444));
        assert!(!ctrl.take_index_pulse());
        tick_index_flag_sync(&mut ctrl);
        assert!(ctrl.take_index_pulse());
        assert_eq!(ctrl.next_disk_word(0, 0), Some(0x1111));

        let _ = fs::remove_file(path);
        Ok(())
    }

    #[test]
    fn scp_flux_image_decodes_read_only_raw_mfm_track() -> Result<()> {
        let raw_words = [0x4489, 0x2AAA];
        let path = temp_scp_raw_revolutions(&[&raw_words], 32)?;
        let cfg = FloppyConfig {
            drives: [
                Some(FloppyDriveConfig {
                    path: path.clone(),
                    write_protected: false,
                }),
                None,
                None,
                None,
            ],
        };
        let mut ctrl = FloppyController::from_config(&cfg)?;

        ctrl.write_prb(!CIAB_DSKMOTOR & !CIAB_DSKSEL0);
        ctrl.tick(MOTOR_READY_CCK, 0, &mut []);
        ctrl.ensure_track(0, 0);

        assert_eq!(ctrl.drives[0].cached_words(), raw_words);
        assert_eq!(ctrl.drives[0].cached_index_words(), raw_words.len());
        assert!(ctrl.drives[0]
            .image
            .as_ref()
            .is_some_and(|image| image.write_protected));

        let _ = fs::remove_file(path);
        Ok(())
    }

    #[test]
    fn scp_flux_decode_resolves_variable_intervals_to_cells() -> Result<()> {
        let path = temp_scp_flux_entries(&[60, 100, 80], 3)?;
        let cfg = FloppyConfig {
            drives: [
                Some(FloppyDriveConfig {
                    path: path.clone(),
                    write_protected: true,
                }),
                None,
                None,
                None,
            ],
        };
        let mut ctrl = FloppyController::from_config(&cfg)?;

        ctrl.write_prb(!CIAB_DSKMOTOR & !CIAB_DSKSEL0);
        ctrl.tick(MOTOR_READY_CCK, 0, &mut []);
        ctrl.ensure_track(0, 0);

        // The PLL resolves the 1500/2500/2000 ns flux intervals at decode time
        // to three consecutive "1" cells. The recovered bits are stored as a
        // single revolution of exact length; the head then clocks them at a
        // uniform per-revolution rate (the captured flux timing is consumed by
        // the data separator, not retained per bit at runtime).
        assert_eq!(ctrl.drives[0].cached.revs.len(), 1);
        assert_eq!(ctrl.drives[0].cached.revs[0].bit_len, 3);
        assert_eq!(ctrl.drives[0].cached_words(), [0xE000]);

        let _ = fs::remove_file(path);
        Ok(())
    }

    #[test]
    fn scp_flux_image_cycles_stored_revolutions() -> Result<()> {
        let rev0 = [0x4489, 0x2AAA];
        let rev1 = [0x5555, 0xA144];
        let path = temp_scp_raw_revolutions(&[&rev0, &rev1], 32)?;
        let cfg = FloppyConfig {
            drives: [
                Some(FloppyDriveConfig {
                    path: path.clone(),
                    write_protected: true,
                }),
                None,
                None,
                None,
            ],
        };
        let mut ctrl = FloppyController::from_config(&cfg)?;

        ctrl.write_prb(!CIAB_DSKMOTOR & !CIAB_DSKSEL0);
        ctrl.tick(MOTOR_READY_CCK, 0, &mut []);
        clear_index_flag(&mut ctrl);
        ctrl.ensure_track(0, 0);
        ctrl.drives[0].set_rotation_word(0);
        ctrl.drives[0].rotation_acc_cck = 0;

        // Per-revolution: each captured revolution is stored separately.
        assert_eq!(ctrl.drives[0].cached.revs.len(), 2);
        assert_eq!(ctrl.drives[0].cached.revs[0].words, rev0);
        assert_eq!(ctrl.drives[0].cached.revs[1].words, rev1);
        assert_eq!(ctrl.drives[0].cached_index_words(), 2);
        let word_cck = FloppyController::word_cck_for_track_words(2);
        assert_eq!(ctrl.next_index_pulse_cck(), Some(word_cck * 2));
        assert_eq!(ctrl.next_disk_word(0, 0), Some(rev0[0]));
        assert_eq!(ctrl.next_disk_word(0, 0), Some(rev0[1]));
        assert!(!ctrl.take_index_pulse());
        tick_index_flag_sync(&mut ctrl);
        assert!(ctrl.take_index_pulse());
        assert_eq!(ctrl.next_disk_word(0, 0), Some(rev1[0]));
        assert_eq!(ctrl.next_disk_word(0, 0), Some(rev1[1]));
        assert!(!ctrl.take_index_pulse());
        tick_index_flag_sync(&mut ctrl);
        assert!(ctrl.take_index_pulse());
        assert_eq!(ctrl.next_disk_word(0, 0), Some(rev0[0]));

        let _ = fs::remove_file(path);
        Ok(())
    }

    #[test]
    fn scp_extended_mode_uses_extended_track_table_offset() -> Result<()> {
        let raw_words = [0x4489, 0x2AAA];
        let path = temp_scp_raw_revolutions_with_flags(
            &[&raw_words],
            32,
            SCP_FLAG_INDEX | SCP_FLAG_EXTENDED_MODE,
        )?;
        let cfg = FloppyConfig {
            drives: [
                Some(FloppyDriveConfig {
                    path: path.clone(),
                    write_protected: true,
                }),
                None,
                None,
                None,
            ],
        };
        let mut ctrl = FloppyController::from_config(&cfg)?;

        ctrl.write_prb(!CIAB_DSKMOTOR & !CIAB_DSKSEL0);
        ctrl.tick(MOTOR_READY_CCK, 0, &mut []);
        ctrl.ensure_track(0, 0);

        assert_eq!(ctrl.drives[0].cached_words(), raw_words);

        let _ = fs::remove_file(path);
        Ok(())
    }

    #[test]
    fn scp_explicit_16_bit_width_keeps_reserved_bytes_as_reserved() -> Result<()> {
        let raw_words = [0x4489, 0x2AAA];
        let path = temp_scp_raw_revolutions(&[&raw_words], 32)?;
        let mut image = fs::read(&path)?;
        image[0x09] = SCP_EXPLICIT_16_BIT_FLUX_WIDTH;
        image[0x0A] = 0x12;
        image[0x0B] = 0x34;
        fs::write(&path, &image)?;

        let cfg = FloppyConfig {
            drives: [
                Some(FloppyDriveConfig {
                    path: path.clone(),
                    write_protected: true,
                }),
                None,
                None,
                None,
            ],
        };
        let mut ctrl = FloppyController::from_config(&cfg)?;

        ctrl.write_prb(!CIAB_DSKMOTOR & !CIAB_DSKSEL0);
        ctrl.tick(MOTOR_READY_CCK, 0, &mut []);
        ctrl.ensure_track(0, 0);

        assert_eq!(ctrl.drives[0].cached_words(), raw_words);
        assert_eq!(ctrl.drives[0].cached_index_words(), raw_words.len());

        let _ = fs::remove_file(path);
        Ok(())
    }

    #[test]
    fn scp_non_indexed_capture_uses_rpm_for_synthetic_index() -> Result<()> {
        assert_eq!(scp_revolution_bit_len(0, 0)?, Some(100_000));
        assert_eq!(scp_revolution_bit_len(0, SCP_FLAG_RPM_360)?, Some(83_333));

        let raw_words = [0x4489, 0x2AAA];
        let path = temp_scp_raw_revolutions_with_flags(&[&raw_words], 32, 0)?;
        let cfg = FloppyConfig {
            drives: [
                Some(FloppyDriveConfig {
                    path: path.clone(),
                    write_protected: true,
                }),
                None,
                None,
                None,
            ],
        };
        let mut ctrl = FloppyController::from_config(&cfg)?;

        ctrl.write_prb(!CIAB_DSKMOTOR & !CIAB_DSKSEL0);
        ctrl.tick(MOTOR_READY_CCK, 0, &mut []);
        ctrl.ensure_track(0, 0);

        assert_eq!(&ctrl.drives[0].cached_words()[..raw_words.len()], raw_words);
        assert_eq!(
            ctrl.drives[0].cached_index_words(),
            100_000usize.div_ceil(16)
        );
        assert_eq!(
            ctrl.drives[0].cached_words().len(),
            100_000usize.div_ceil(16)
        );

        let _ = fs::remove_file(path);
        Ok(())
    }

    #[test]
    fn scp_checksum_is_verified_when_present() -> Result<()> {
        let raw_words = [0x4489, 0x2AAA];
        let path = temp_scp_raw_revolutions(&[&raw_words], 32)?;
        let mut image = fs::read(&path)?;
        write_scp_checksum(&mut image);
        fs::write(&path, &image)?;

        let cfg = FloppyConfig {
            drives: [
                Some(FloppyDriveConfig {
                    path: path.clone(),
                    write_protected: true,
                }),
                None,
                None,
                None,
            ],
        };
        let mut ctrl = FloppyController::from_config(&cfg)?;
        ctrl.write_prb(!CIAB_DSKMOTOR & !CIAB_DSKSEL0);
        ctrl.tick(MOTOR_READY_CCK, 0, &mut []);
        ctrl.ensure_track(0, 0);
        assert_eq!(ctrl.drives[0].cached_words(), raw_words);

        let last = image.len() - 1;
        image[last] ^= 0x01;
        fs::write(&path, &image)?;
        let err = FloppyController::from_config(&cfg)
            .err()
            .expect("corrupt SCP checksum should fail");
        assert!(format!("{err:#}").contains("SCP checksum mismatch"));

        let _ = fs::remove_file(path);
        Ok(())
    }

    #[test]
    fn scp_flux_zero_entries_extend_transition_intervals() -> Result<()> {
        let payload = [0x00, 0x00, 0x00, 0x01];
        let (words, bit_len, bitcell_ns) =
            scp_flux_to_mfm_words(0, 0, &payload, SCP_CAPTURE_BASE_NS, None)?;

        assert_eq!(bit_len, 819);
        assert_eq!(bitcell_ns.len(), bit_len as usize);
        assert_eq!(words.len(), 52);
        assert_eq!(words.last().copied().unwrap() & 0x2000, 0x2000);
        Ok(())
    }

    #[test]
    fn scp_flux_pll_decodes_each_interval_locally_without_drift() -> Result<()> {
        // Two equal 2500 ns intervals (0x64 ticks each). The PLL data separator
        // resolves each interval locally as round(2500/cell) = 1 cell, so the
        // two flux transitions decode to two consecutive "1" cells carrying
        // their measured 2500 ns time. The old cumulative quantizer instead
        // accumulated the 0.25-cell remainder and emitted "101" (3 bits) -- the
        // drift this PLL removes.
        let payload = [0x00, 0x64, 0x00, 0x64];
        let (words, bit_len, bitcell_ns) =
            scp_flux_to_mfm_words(0, 0, &payload, SCP_CAPTURE_BASE_NS, None)?;

        assert_eq!(bit_len, 2);
        assert_eq!(bitcell_ns, [2500, 2500]);
        assert_eq!(words[0] & 0xC000, 0xC000);
        Ok(())
    }

    #[test]
    fn scp_flux_pll_locks_to_offnominal_rate_without_drift() -> Result<()> {
        // A real disk's cell rate is rarely exactly 2 us. Simulate a track
        // spinning slightly fast: 40 flux transitions exactly two cells apart
        // at a 1950 ns cell (3900 ns = 156 SCP ticks per interval). The PLL
        // must lock to 1950 ns and resolve every interval as exactly 2 cells,
        // recovering a clean alternating "01" stream with no accumulated drift.
        // (A fixed-2 us cumulative quantizer drifts ~1 cell every ~13 intervals
        // and would mis-resolve later transitions, corrupting the stream.)
        let payload: Vec<u8> = std::iter::repeat_n([0x00, 0x9C], 40).flatten().collect();
        let (words, bit_len, bitcell_ns) =
            scp_flux_to_mfm_words(0, 0, &payload, SCP_CAPTURE_BASE_NS, None)?;

        assert_eq!(bit_len, 80, "40 two-cell intervals => 80 cells, no drift");
        assert!(
            bitcell_ns.iter().all(|&ns| ns == 1950),
            "PLL should recover a uniform 1950 ns cell"
        );
        // Each interval is one "0" cell then one "1" cell => 0b0101... = 0x5555.
        assert_eq!(words[0], 0x5555);
        assert_eq!(words[1], 0x5555);
        Ok(())
    }

    #[test]
    fn scp_flux_preserves_uneven_bitcell_timing() -> Result<()> {
        let payload = [0x00, 0x3C, 0x00, 0x64, 0x00, 0x50];
        let (words, bit_len, bitcell_ns) =
            scp_flux_to_mfm_words(0, 0, &payload, SCP_CAPTURE_BASE_NS, None)?;

        assert_eq!(bit_len, 3);
        assert_eq!(bitcell_ns, [1500, 2500, 2000]);
        assert_eq!(words[0] & 0xE000, 0xE000);
        Ok(())
    }

    #[test]
    fn extended_track_length_controls_index_timing() -> Result<()> {
        let raw_words = [0x1111, 0x2222, 0x3333, 0x4444];
        let path = temp_ext2_raw(&raw_words)?;
        let cfg = FloppyConfig {
            drives: [
                Some(FloppyDriveConfig {
                    path: path.clone(),
                    write_protected: true,
                }),
                None,
                None,
                None,
            ],
        };
        let mut ctrl = FloppyController::from_config(&cfg)?;

        ctrl.write_prb(!CIAB_DSKMOTOR & !CIAB_DSKSEL0);
        ctrl.tick(MOTOR_READY_CCK, 0, &mut []);
        clear_index_flag(&mut ctrl);
        ctrl.ensure_track(0, 0);
        ctrl.drives[0].set_rotation_word(raw_words.len() - 1);
        ctrl.drives[0].rotation_acc_cck = 0;

        let word_cck = FloppyController::word_cck_for_track_words(raw_words.len());
        let remaining_cck = word_cck
            .saturating_sub(ctrl.drives[0].rotation_acc_cck)
            .max(1);
        assert_eq!(ctrl.next_index_pulse_cck(), Some(remaining_cck));

        ctrl.tick(word_cck, 0, &mut []);
        assert!(!ctrl.take_index_pulse());
        tick_index_flag_sync(&mut ctrl);
        assert!(ctrl.take_index_pulse());

        let _ = fs::remove_file(path);
        Ok(())
    }

    #[test]
    fn one_word_raw_tracks_do_not_advertise_index_deadlines() -> Result<()> {
        let raw_words = [0x4489];
        let path = temp_ext2_raw(&raw_words)?;
        let cfg = FloppyConfig {
            drives: [
                Some(FloppyDriveConfig {
                    path: path.clone(),
                    write_protected: true,
                }),
                None,
                None,
                None,
            ],
        };
        let mut ctrl = FloppyController::from_config(&cfg)?;

        ctrl.write_prb(!CIAB_DSKMOTOR & !CIAB_DSKSEL0);
        ctrl.tick(MOTOR_READY_CCK, 0, &mut []);
        clear_index_flag(&mut ctrl);
        ctrl.ensure_track(0, 0);

        assert_eq!(ctrl.next_index_pulse_cck(), None);
        ctrl.tick(FloppyController::word_cck_for_track_words(1), 0, &mut []);
        assert!(!ctrl.index_pulse_active());
        assert!(!ctrl.take_index_pulse());

        let _ = fs::remove_file(path);
        Ok(())
    }

    #[test]
    fn raw_track_write_dma_uses_raw_index_timing() -> Result<()> {
        let raw_words = [0x1111, 0x2222, 0x3333, 0x4444];
        let path = temp_ext2_raw(&raw_words)?;
        let cfg = FloppyConfig {
            drives: [
                Some(FloppyDriveConfig {
                    path: path.clone(),
                    write_protected: false,
                }),
                None,
                None,
                None,
            ],
        };
        let mut ctrl = FloppyController::from_config(&cfg)?;
        let mut chip_ram = vec![0xAB, 0xCD];

        ctrl.write_prb(!CIAB_DSKMOTOR & !CIAB_DSKSEL0);
        ctrl.tick(MOTOR_READY_CCK, 0, &mut chip_ram);
        clear_index_flag(&mut ctrl);
        ctrl.ensure_track(0, 0);
        ctrl.drives[0].set_rotation_word(raw_words.len() - 1);
        ctrl.drives[0].rotation_acc_cck = 0;
        ctrl.set_dskpt_low(0);
        let len = DSKLEN_DMAEN | DSKLEN_WRITE | 1;
        assert!(!ctrl.write_dsklen(len, 0));
        assert!(!ctrl.write_dsklen(len, 0));

        let dmacon = DMACON_DMAEN | DMACON_DISK;
        let word_cck = FloppyController::word_cck_for_track_words(raw_words.len());
        assert!(ctrl.tick(word_cck, dmacon, &mut chip_ram));
        assert!(!ctrl.take_index_pulse());
        tick_index_flag_sync(&mut ctrl);
        assert!(ctrl.take_index_pulse());
        assert_eq!(ctrl.drives[0].rotation_word_index(), 0);

        let _ = fs::remove_file(path);
        Ok(())
    }

    #[test]
    fn cpu_dskdat_write_without_dma_overlays_raw_track() -> Result<()> {
        let raw_words = [0x1111, 0x2222, 0x3333, 0x4444];
        let path = temp_ext2_raw(&raw_words)?;
        let cfg = FloppyConfig {
            drives: [
                Some(FloppyDriveConfig {
                    path: path.clone(),
                    write_protected: false,
                }),
                None,
                None,
                None,
            ],
        };
        let mut ctrl = FloppyController::from_config(&cfg)?;

        ctrl.write_prb(!CIAB_DSKMOTOR & !CIAB_DSKSEL0);
        ctrl.tick(MOTOR_READY_CCK, 0, &mut []);
        ctrl.ensure_track(0, 0);
        ctrl.drives[0].set_rotation_word(1);
        ctrl.drives[0].rotation_acc_cck = 0;

        assert!(!ctrl.write_dsklen(DSKLEN_WRITE | 1, 0));
        let status = ctrl.read_dskbytr(0, 0);
        assert_eq!(status & DMAON, 0);
        assert_ne!(status & DISKWRITE, 0);

        ctrl.write_dskdat(0xABCD);

        let mut reloaded = FloppyController::from_config(&cfg)?;
        reloaded.write_prb(!CIAB_DSKMOTOR & !CIAB_DSKSEL0);
        reloaded.tick(MOTOR_READY_CCK, 0, &mut []);
        reloaded.ensure_track(0, 0);
        assert_eq!(
            reloaded.drives[0].cached_words(),
            [0x1111, 0xABCA, 0x3333, 0x4444]
        );

        let _ = fs::remove_file(path);
        Ok(())
    }

    #[test]
    fn uae_extended_adf_amigados_track_encodes_sectors() -> Result<()> {
        let mut track_data = vec![0u8; SECTORS_PER_TRACK * BYTES_PER_SECTOR];
        track_data[0..BYTES_PER_SECTOR].fill(0x5A);
        let path = temp_ext2_amigados(&track_data)?;
        let cfg = FloppyConfig {
            drives: [
                Some(FloppyDriveConfig {
                    path: path.clone(),
                    write_protected: true,
                }),
                None,
                None,
                None,
            ],
        };
        let mut ctrl = FloppyController::from_config(&cfg)?;

        ctrl.write_prb(!CIAB_DSKMOTOR & !CIAB_DSKSEL0);
        ctrl.tick(MOTOR_READY_CCK, 0, &mut []);
        ctrl.ensure_track(0, 0);
        let decoded = decode_track_write(0, &ctrl.drives[0].cached_words())?;

        let sector0 = decoded.iter().find(|(sector, _)| *sector == 0).unwrap();
        assert_eq!(&sector0.1[..], &[0x5A; BYTES_PER_SECTOR]);
        assert_eq!(decoded.len(), SECTORS_PER_TRACK);

        let _ = fs::remove_file(path);
        Ok(())
    }

    #[test]
    fn uae_extended_adf_amigados_track_uses_bit_length_before_zero_padding() -> Result<()> {
        let mut track_data = vec![0u8; SECTORS_PER_TRACK * BYTES_PER_SECTOR];
        track_data[0..BYTES_PER_SECTOR].fill(0x5A);
        let mut payload = track_data.clone();
        payload.resize(0x31f0, 0);
        let path = temp_ext2_track(0, (track_data.len() * 8) as u32, &payload)?;
        let cfg = FloppyConfig {
            drives: [
                Some(FloppyDriveConfig {
                    path: path.clone(),
                    write_protected: true,
                }),
                None,
                None,
                None,
            ],
        };
        let mut ctrl = FloppyController::from_config(&cfg)?;

        ctrl.write_prb(!CIAB_DSKMOTOR & !CIAB_DSKSEL0);
        ctrl.tick(MOTOR_READY_CCK, 0, &mut []);
        ctrl.ensure_track(0, 0);
        let decoded = decode_track_write(0, &ctrl.drives[0].cached_words())?;

        let sector0 = decoded.iter().find(|(sector, _)| *sector == 0).unwrap();
        assert_eq!(&sector0.1[..], &[0x5A; BYTES_PER_SECTOR]);
        assert_eq!(decoded.len(), SECTORS_PER_TRACK);

        let _ = fs::remove_file(path);
        Ok(())
    }

    #[test]
    fn uae_extended_adf_amigados_track_rejects_nonzero_padding_after_bit_length() {
        let track_data = vec![0u8; SECTORS_PER_TRACK * BYTES_PER_SECTOR];
        let mut payload = track_data.clone();
        payload.resize(0x31f0, 0);
        *payload.last_mut().unwrap() = 0x5A;
        let image = ext2_track_image(0, (track_data.len() * 8) as u32, 1, &payload);

        let err = match decode_uae_extended_adf(&image) {
            Ok(_) => panic!("non-zero padding after AmigaDOS bit length should fail"),
            Err(err) => err,
        };
        assert!(err
            .to_string()
            .contains("AmigaDOS padding after bit length is non-zero"));
    }

    #[test]
    fn uae_extended_adf_blank_amigados_track_without_sector_payload_is_empty() -> Result<()> {
        let payload = vec![0u8; 0x31f0];
        let image = ext2_track_image(0, (payload.len() * 8) as u32, 1, &payload);

        let FloppyImageData::Tracks(tracks) = decode_uae_extended_adf(&image)? else {
            panic!("UAE extended ADF should decode to per-track data");
        };
        assert!(tracks[0].is_none());
        Ok(())
    }

    #[test]
    fn writable_extended_adf_amigados_track_persists_sector_updates() -> Result<()> {
        let path = temp_ext2_amigados(&vec![0u8; SECTORS_PER_TRACK * BYTES_PER_SECTOR])?;
        let cfg = FloppyConfig {
            drives: [
                Some(FloppyDriveConfig {
                    path: path.clone(),
                    write_protected: false,
                }),
                None,
                None,
                None,
            ],
        };
        let mut ctrl = FloppyController::from_config(&cfg)?;
        let mut source = vec![0u8; SECTORS_PER_TRACK * BYTES_PER_SECTOR];
        source[0..BYTES_PER_SECTOR].fill(0xA5);
        let words = encode_amigados_track(0, &source);
        let mut chip_ram = vec![0u8; words.len() * 2 + 2];
        for (i, word) in words.iter().copied().enumerate() {
            let [hi, lo] = word.to_be_bytes();
            chip_ram[i * 2] = hi;
            chip_ram[i * 2 + 1] = lo;
        }

        ctrl.write_prb(!CIAB_DSKMOTOR & !CIAB_DSKSEL0);
        ctrl.tick(MOTOR_READY_CCK, 0, &mut chip_ram);
        ctrl.set_dskpt_low(0);
        let len = DSKLEN_DMAEN | DSKLEN_WRITE | (words.len() as u16 & DSKLEN_MASK);
        assert!(!ctrl.write_dsklen(len, 0));
        assert!(!ctrl.write_dsklen(len, 0));
        let dmacon = DMACON_DMAEN | DMACON_DISK;
        while !ctrl.tick(ctrl.word_cck(), dmacon, &mut chip_ram) {}

        let persisted = fs::read(&path)?;
        assert_eq!(&persisted[0..8], UAE_EXT2_SIGNATURE);
        let payload_off = 8 + 4 + 12;
        assert_eq!(
            &persisted[payload_off..payload_off + BYTES_PER_SECTOR],
            &[0xA5; BYTES_PER_SECTOR]
        );

        let mut reloaded = FloppyController::from_config(&cfg)?;
        reloaded.write_prb(!CIAB_DSKMOTOR & !CIAB_DSKSEL0);
        reloaded.tick(MOTOR_READY_CCK, 0, &mut []);
        reloaded.ensure_track(0, 0);
        let decoded = decode_track_write(0, &reloaded.drives[0].cached_words())?;
        let sector0 = decoded.iter().find(|(sector, _)| *sector == 0).unwrap();
        assert_eq!(&sector0.1[..], &[0xA5; BYTES_PER_SECTOR]);

        let _ = fs::remove_file(path);
        Ok(())
    }

    #[test]
    fn writable_extended_adf_preserves_multi_revolution_raw_track_payload() -> Result<()> {
        let raw_words: [u16; 4] = [0x1111, 0x2222, 0x3333, 0x4444];
        let raw_payload: Vec<u8> = raw_words
            .iter()
            .copied()
            .flat_map(u16::to_be_bytes)
            .collect();
        let path = temp_ext2_amigados_plus_raw(
            &vec![0u8; SECTORS_PER_TRACK * BYTES_PER_SECTOR],
            &raw_words,
            32,
            2,
        )?;
        let cfg = FloppyConfig {
            drives: [
                Some(FloppyDriveConfig {
                    path: path.clone(),
                    write_protected: false,
                }),
                None,
                None,
                None,
            ],
        };
        let mut ctrl = FloppyController::from_config(&cfg)?;
        let mut source = vec![0u8; SECTORS_PER_TRACK * BYTES_PER_SECTOR];
        source[0..BYTES_PER_SECTOR].fill(0xA5);
        let words = encode_amigados_track(0, &source);
        let mut chip_ram = vec![0u8; words.len() * 2 + 2];
        for (i, word) in words.iter().copied().enumerate() {
            let [hi, lo] = word.to_be_bytes();
            chip_ram[i * 2] = hi;
            chip_ram[i * 2 + 1] = lo;
        }

        ctrl.write_prb(!CIAB_DSKMOTOR & !CIAB_DSKSEL0);
        ctrl.tick(MOTOR_READY_CCK, 0, &mut chip_ram);
        ctrl.set_dskpt_low(0);
        let len = DSKLEN_DMAEN | DSKLEN_WRITE | (words.len() as u16 & DSKLEN_MASK);
        assert!(!ctrl.write_dsklen(len, 0));
        assert!(!ctrl.write_dsklen(len, 0));
        let dmacon = DMACON_DMAEN | DMACON_DISK;
        while !ctrl.tick(ctrl.word_cck(), dmacon, &mut chip_ram) {}

        let persisted = fs::read(&path)?;
        let track0_len = SECTORS_PER_TRACK * BYTES_PER_SECTOR;
        let raw_desc = &persisted[24..36];
        assert_eq!(raw_desc[2], 1);
        assert_eq!(raw_desc[3], 1);
        assert_eq!(
            u32::from_be_bytes([raw_desc[4], raw_desc[5], raw_desc[6], raw_desc[7]]) as usize,
            raw_payload.len()
        );
        assert_eq!(
            u32::from_be_bytes([raw_desc[8], raw_desc[9], raw_desc[10], raw_desc[11]]),
            32
        );
        let raw_payload_off = 12 + 2 * 12 + track0_len;
        assert_eq!(
            &persisted[raw_payload_off..raw_payload_off + raw_payload.len()],
            &raw_payload[..]
        );

        let _ = fs::remove_file(path);
        Ok(())
    }

    #[test]
    fn writable_extended_adf_raw_track_overlays_word_stream() -> Result<()> {
        let raw_words = [0x4489, 0x2AAA, 0x5555, 0xA144];
        let path = temp_ext2_raw(&raw_words)?;
        let cfg = FloppyConfig {
            drives: [
                Some(FloppyDriveConfig {
                    path: path.clone(),
                    write_protected: false,
                }),
                None,
                None,
                None,
            ],
        };
        let mut ctrl = FloppyController::from_config(&cfg)?;
        let words: [u16; 2] = [0xDEAD, 0xBEEF];
        let mut chip_ram = vec![0u8; words.len() * 2 + 2];
        for (i, word) in words.iter().copied().enumerate() {
            let [hi, lo] = word.to_be_bytes();
            chip_ram[i * 2] = hi;
            chip_ram[i * 2 + 1] = lo;
        }

        ctrl.write_prb(!CIAB_DSKMOTOR & !CIAB_DSKSEL0);
        ctrl.tick(MOTOR_READY_CCK, 0, &mut chip_ram);
        ctrl.ensure_track(0, 0);
        ctrl.drives[0].set_rotation_word(2);
        ctrl.drives[0].rotation_acc_cck = 0;
        ctrl.set_dskpt_low(0);
        let len = DSKLEN_DMAEN | DSKLEN_WRITE | words.len() as u16;
        assert!(!ctrl.write_dsklen(len, 0));
        assert!(!ctrl.write_dsklen(len, 0));
        let dmacon = DMACON_DMAEN | DMACON_DISK;
        while !ctrl.tick(ctrl.word_cck(), dmacon, &mut chip_ram) {}

        let persisted = fs::read(&path)?;
        let desc = &persisted[12..24];
        assert_eq!(desc[2], 0);
        assert_eq!(desc[3], 1);
        assert_eq!(
            u32::from_be_bytes([desc[4], desc[5], desc[6], desc[7]]) as usize,
            raw_words.len() * 2
        );
        assert_eq!(
            u32::from_be_bytes([desc[8], desc[9], desc[10], desc[11]]),
            64
        );
        assert_eq!(
            &persisted[24..32],
            &[0x44, 0x89, 0x2A, 0xAA, 0xDE, 0xAD, 0xBE, 0xEC]
        );

        let mut reloaded = FloppyController::from_config(&cfg)?;
        reloaded.write_prb(!CIAB_DSKMOTOR & !CIAB_DSKSEL0);
        reloaded.tick(MOTOR_READY_CCK, 0, &mut []);
        reloaded.ensure_track(0, 0);
        assert_eq!(
            reloaded.drives[0].cached_words(),
            [0x4489, 0x2AAA, 0xDEAD, 0xBEEC]
        );

        let _ = fs::remove_file(path);
        Ok(())
    }

    #[test]
    fn writable_extended_adf_raw_track_overlays_bit_phase() -> Result<()> {
        let raw_words = [0x0000, 0x0000, 0x0000, 0x0000];
        let path = temp_ext2_raw(&raw_words)?;
        let cfg = FloppyConfig {
            drives: [
                Some(FloppyDriveConfig {
                    path: path.clone(),
                    write_protected: false,
                }),
                None,
                None,
                None,
            ],
        };
        let mut ctrl = FloppyController::from_config(&cfg)?;
        let mut chip_ram = vec![0u8; 64];
        chip_ram[0..2].copy_from_slice(&0xFFFFu16.to_be_bytes());

        ctrl.write_prb(!CIAB_DSKMOTOR & !CIAB_DSKSEL0);
        ctrl.tick(MOTOR_READY_CCK, 0, &mut chip_ram);
        ctrl.ensure_track(0, 0);
        // Head at word 1, 8 cells in (bit 24) -- the write's landing bit phase.
        ctrl.drives[0].set_rotation_bit(24);
        let word_cck = FloppyController::word_cck_for_track_words(raw_words.len());
        ctrl.set_dskpt_low(0);
        let len = DSKLEN_DMAEN | DSKLEN_WRITE | 1;
        assert!(!ctrl.write_dsklen(len, 0));
        assert!(!ctrl.write_dsklen(len, 0));
        let dmacon = DMACON_DMAEN | DMACON_DISK;
        while !ctrl.tick(word_cck, dmacon, &mut chip_ram) {}

        let mut reloaded = FloppyController::from_config(&cfg)?;
        reloaded.write_prb(!CIAB_DSKMOTOR & !CIAB_DSKSEL0);
        reloaded.tick(MOTOR_READY_CCK, 0, &mut []);
        reloaded.ensure_track(0, 0);
        assert_eq!(
            reloaded.drives[0].cached_words(),
            [0x0000, 0x00FF, 0xF800, 0x0000]
        );

        let _ = fs::remove_file(path);
        Ok(())
    }

    #[test]
    fn writable_extended_adf_raw_track_wraps_at_partial_bit_len() -> Result<()> {
        let raw_words = [0x0000, 0x0000];
        let path = temp_ext2_raw_revolutions(&raw_words, 20, 1)?;
        let cfg = FloppyConfig {
            drives: [
                Some(FloppyDriveConfig {
                    path: path.clone(),
                    write_protected: false,
                }),
                None,
                None,
                None,
            ],
        };
        let mut ctrl = FloppyController::from_config(&cfg)?;
        let mut chip_ram = vec![0u8; 64];
        chip_ram[0..2].copy_from_slice(&0xFFFFu16.to_be_bytes());

        ctrl.write_prb(!CIAB_DSKMOTOR & !CIAB_DSKSEL0);
        ctrl.tick(MOTOR_READY_CCK, 0, &mut chip_ram);
        ctrl.ensure_track(0, 0);
        // Head at word 1, 4 cells in (bit 20 == bit_len, wraps to the start).
        ctrl.drives[0].set_rotation_bit(20);
        let word_cck = FloppyController::word_cck_for_track_words(raw_words.len());
        ctrl.set_dskpt_low(0);
        let len = DSKLEN_DMAEN | DSKLEN_WRITE | 1;
        assert!(!ctrl.write_dsklen(len, 0));
        assert!(!ctrl.write_dsklen(len, 0));
        let dmacon = DMACON_DMAEN | DMACON_DISK;
        while !ctrl.tick(word_cck, dmacon, &mut chip_ram) {}

        let persisted = fs::read(&path)?;
        let desc = &persisted[12..24];
        assert_eq!(
            u32::from_be_bytes([desc[8], desc[9], desc[10], desc[11]]),
            20
        );
        assert_eq!(&persisted[24..28], &[0xFF, 0xF8, 0x00, 0x00]);

        let mut reloaded = FloppyController::from_config(&cfg)?;
        reloaded.write_prb(!CIAB_DSKMOTOR & !CIAB_DSKSEL0);
        reloaded.tick(MOTOR_READY_CCK, 0, &mut []);
        reloaded.ensure_track(0, 0);
        assert_eq!(reloaded.drives[0].cached_words(), [0xFFF8, 0x0000]);

        let _ = fs::remove_file(path);
        Ok(())
    }

    #[test]
    fn writable_extended_adf_raw_track_preserves_partial_word_tail() -> Result<()> {
        let path = temp_ext2_track(1, 20, &[0xFF, 0xFF, 0xF0])?;
        let cfg = FloppyConfig {
            drives: [
                Some(FloppyDriveConfig {
                    path: path.clone(),
                    write_protected: false,
                }),
                None,
                None,
                None,
            ],
        };
        let mut ctrl = FloppyController::from_config(&cfg)?;
        let mut chip_ram = vec![0u8; 2];

        ctrl.write_prb(!CIAB_DSKMOTOR & !CIAB_DSKSEL0);
        ctrl.tick(MOTOR_READY_CCK, 0, &mut chip_ram);
        ctrl.ensure_track(0, 0);
        assert_eq!(ctrl.drives[0].cached_words(), [0xFFFF, 0xF000]);
        ctrl.drives[0].set_rotation_word(0);
        ctrl.drives[0].rotation_acc_cck = 0;
        ctrl.set_dskpt_low(0);
        let len = DSKLEN_DMAEN | DSKLEN_WRITE | 1;
        assert!(!ctrl.write_dsklen(len, 0));
        assert!(!ctrl.write_dsklen(len, 0));
        let dmacon = DMACON_DMAEN | DMACON_DISK;
        while !ctrl.tick(ctrl.word_cck(), dmacon, &mut chip_ram) {}

        let persisted = fs::read(&path)?;
        let desc = &persisted[12..24];
        assert_eq!(
            u32::from_be_bytes([desc[4], desc[5], desc[6], desc[7]]) as usize,
            3
        );
        assert_eq!(
            u32::from_be_bytes([desc[8], desc[9], desc[10], desc[11]]),
            20
        );
        assert_eq!(&persisted[24..27], &[0x00, 0x07, 0xF0]);

        let mut reloaded = FloppyController::from_config(&cfg)?;
        reloaded.write_prb(!CIAB_DSKMOTOR & !CIAB_DSKSEL0);
        reloaded.tick(MOTOR_READY_CCK, 0, &mut []);
        reloaded.ensure_track(0, 0);
        assert_eq!(reloaded.drives[0].cached_words(), [0x0007, 0xF000]);

        let _ = fs::remove_file(path);
        Ok(())
    }

    #[test]
    fn raw_track_write_dma_loses_last_three_output_bits() -> Result<()> {
        let raw_words = [0xFFFF];
        let path = temp_ext2_raw(&raw_words)?;
        let cfg = FloppyConfig {
            drives: [
                Some(FloppyDriveConfig {
                    path: path.clone(),
                    write_protected: false,
                }),
                None,
                None,
                None,
            ],
        };
        let mut ctrl = FloppyController::from_config(&cfg)?;
        let mut chip_ram = vec![0x00, 0x00];

        ctrl.write_prb(!CIAB_DSKMOTOR & !CIAB_DSKSEL0);
        ctrl.tick(MOTOR_READY_CCK, 0, &mut chip_ram);
        ctrl.ensure_track(0, 0);
        ctrl.drives[0].set_rotation_word(0);
        ctrl.drives[0].rotation_acc_cck = 0;
        ctrl.set_dskpt_low(0);
        let len = DSKLEN_DMAEN | DSKLEN_WRITE | 1;
        assert!(!ctrl.write_dsklen(len, 0));
        assert!(!ctrl.write_dsklen(len, 0));
        let dmacon = DMACON_DMAEN | DMACON_DISK;
        assert!(ctrl.tick(
            FloppyController::word_cck_for_track_words(raw_words.len()),
            dmacon,
            &mut chip_ram
        ));

        let mut reloaded = FloppyController::from_config(&cfg)?;
        reloaded.write_prb(!CIAB_DSKMOTOR & !CIAB_DSKSEL0);
        reloaded.tick(MOTOR_READY_CCK, 0, &mut []);
        reloaded.ensure_track(0, 0);
        assert_eq!(reloaded.drives[0].cached_words(), [0x0007]);

        let _ = fs::remove_file(path);
        Ok(())
    }

    #[test]
    fn protected_raw_track_write_dma_leaves_media_unchanged() -> Result<()> {
        let raw_words = [0x1111, 0x2222];
        let path = temp_ext2_raw(&raw_words)?;
        let cfg = FloppyConfig {
            drives: [
                Some(FloppyDriveConfig {
                    path: path.clone(),
                    write_protected: true,
                }),
                None,
                None,
                None,
            ],
        };
        let mut ctrl = FloppyController::from_config(&cfg)?;
        let mut chip_ram = vec![0xAA, 0xAA];

        ctrl.write_prb(!CIAB_DSKMOTOR & !CIAB_DSKSEL0);
        ctrl.tick(MOTOR_READY_CCK, 0, &mut chip_ram);
        ctrl.ensure_track(0, 0);
        ctrl.drives[0].set_rotation_word(0);
        ctrl.drives[0].rotation_acc_cck = 0;
        ctrl.set_dskpt_low(0);
        let len = DSKLEN_DMAEN | DSKLEN_WRITE | 1;
        assert!(!ctrl.write_dsklen(len, 0));
        assert!(!ctrl.write_dsklen(len, 0));
        let dmacon = DMACON_DMAEN | DMACON_DISK;
        assert!(ctrl.tick(
            FloppyController::word_cck_for_track_words(raw_words.len()),
            dmacon,
            &mut chip_ram
        ));

        let mut reloaded = FloppyController::from_config(&cfg)?;
        reloaded.write_prb(!CIAB_DSKMOTOR & !CIAB_DSKSEL0);
        reloaded.tick(MOTOR_READY_CCK, 0, &mut []);
        reloaded.ensure_track(0, 0);
        assert_eq!(reloaded.drives[0].cached_words(), raw_words);

        let _ = fs::remove_file(path);
        Ok(())
    }

    #[test]
    fn raw_write_dma_abort_persists_captured_words() -> Result<()> {
        let raw_words = [0x1111, 0x2222, 0x3333, 0x4444];
        let path = temp_ext2_raw(&raw_words)?;
        let cfg = FloppyConfig {
            drives: [
                Some(FloppyDriveConfig {
                    path: path.clone(),
                    write_protected: false,
                }),
                None,
                None,
                None,
            ],
        };
        let mut ctrl = FloppyController::from_config(&cfg)?;
        let mut chip_ram = vec![0u8; 64];
        chip_ram[0..2].copy_from_slice(&0xAAAAu16.to_be_bytes());

        ctrl.write_prb(!CIAB_DSKMOTOR & !CIAB_DSKSEL0);
        ctrl.tick(MOTOR_READY_CCK, 0, &mut chip_ram);
        ctrl.ensure_track(0, 0);
        ctrl.drives[0].set_rotation_word(1);
        ctrl.drives[0].rotation_acc_cck = 0;

        let len = DSKLEN_DMAEN | DSKLEN_WRITE | 3;
        assert!(!ctrl.write_dsklen(len, 0));
        assert!(!ctrl.write_dsklen(len, 0));
        let dmacon = DMACON_DMAEN | DMACON_DISK;
        let word_cck = FloppyController::word_cck_for_track_words(raw_words.len());
        assert!(!ctrl.tick(word_cck, dmacon, &mut chip_ram));
        assert!(!ctrl.write_dsklen(0, 0));

        let mut reloaded = FloppyController::from_config(&cfg)?;
        reloaded.write_prb(!CIAB_DSKMOTOR & !CIAB_DSKSEL0);
        reloaded.tick(MOTOR_READY_CCK, 0, &mut []);
        reloaded.ensure_track(0, 0);
        assert_eq!(
            reloaded.drives[0].cached_words(),
            [0x1111, 0xAAAA, 0x3333, 0x4444]
        );

        let _ = fs::remove_file(path);
        Ok(())
    }

    #[test]
    fn active_write_dma_dsklen_rewrite_updates_remaining_length() -> Result<()> {
        let raw_words = [0x1111, 0x2222, 0x3333, 0x4444];
        let path = temp_ext2_raw(&raw_words)?;
        let cfg = FloppyConfig {
            drives: [
                Some(FloppyDriveConfig {
                    path: path.clone(),
                    write_protected: false,
                }),
                None,
                None,
                None,
            ],
        };
        let mut ctrl = FloppyController::from_config(&cfg)?;
        let words = [0xAAAAu16, 0xBBBB, 0xCCCC];
        let mut chip_ram = vec![0u8; words.len() * 2];
        for (i, word) in words.iter().copied().enumerate() {
            let [hi, lo] = word.to_be_bytes();
            chip_ram[i * 2] = hi;
            chip_ram[i * 2 + 1] = lo;
        }

        ctrl.write_prb(!CIAB_DSKMOTOR & !CIAB_DSKSEL0);
        ctrl.tick(MOTOR_READY_CCK, 0, &mut chip_ram);
        ctrl.ensure_track(0, 0);
        ctrl.drives[0].set_rotation_word(1);
        ctrl.drives[0].rotation_acc_cck = 0;
        ctrl.set_dskpt_low(0);

        let len = DSKLEN_DMAEN | DSKLEN_WRITE | 3;
        assert!(!ctrl.write_dsklen(len, 0));
        assert!(!ctrl.write_dsklen(len, 0));
        let dmacon = DMACON_DMAEN | DMACON_DISK;
        let word_cck = FloppyController::word_cck_for_track_words(raw_words.len());
        assert!(!ctrl.tick(word_cck, dmacon, &mut chip_ram));

        let shorter = DSKLEN_DMAEN | DSKLEN_WRITE | 1;
        assert!(!ctrl.write_dsklen(shorter, 0));
        assert!(!ctrl.write_dsklen(shorter, 0));
        assert!(ctrl.tick(word_cck, dmacon, &mut chip_ram));

        let mut reloaded = FloppyController::from_config(&cfg)?;
        reloaded.write_prb(!CIAB_DSKMOTOR & !CIAB_DSKSEL0);
        reloaded.tick(MOTOR_READY_CCK, 0, &mut []);
        reloaded.ensure_track(0, 0);
        assert_eq!(
            reloaded.drives[0].cached_words(),
            [0x1111, 0xAAAA, 0xBBBB, 0x4444]
        );

        let _ = fs::remove_file(path);
        Ok(())
    }

    #[test]
    fn active_write_dma_dsklen_zero_rewrite_finishes_now() -> Result<()> {
        let raw_words = [0x1111, 0x2222, 0x3333, 0x4444];
        let path = temp_ext2_raw(&raw_words)?;
        let cfg = FloppyConfig {
            drives: [
                Some(FloppyDriveConfig {
                    path: path.clone(),
                    write_protected: false,
                }),
                None,
                None,
                None,
            ],
        };
        let mut ctrl = FloppyController::from_config(&cfg)?;
        let mut chip_ram = vec![0u8; 4];
        chip_ram[0..2].copy_from_slice(&0xAAAAu16.to_be_bytes());
        chip_ram[2..4].copy_from_slice(&0xBBBBu16.to_be_bytes());

        ctrl.write_prb(!CIAB_DSKMOTOR & !CIAB_DSKSEL0);
        ctrl.tick(MOTOR_READY_CCK, 0, &mut chip_ram);
        ctrl.ensure_track(0, 0);
        ctrl.drives[0].set_rotation_word(1);
        ctrl.drives[0].rotation_acc_cck = 0;
        ctrl.set_dskpt_low(0);

        let len = DSKLEN_DMAEN | DSKLEN_WRITE | 2;
        assert!(!ctrl.write_dsklen(len, 0));
        assert!(!ctrl.write_dsklen(len, 0));
        let dmacon = DMACON_DMAEN | DMACON_DISK;
        let word_cck = FloppyController::word_cck_for_track_words(raw_words.len());
        assert!(!ctrl.tick(word_cck, dmacon, &mut chip_ram));

        assert!(ctrl.write_dsklen(DSKLEN_DMAEN | DSKLEN_WRITE, 0));
        assert!(!ctrl.tick(word_cck, dmacon, &mut chip_ram));

        let mut reloaded = FloppyController::from_config(&cfg)?;
        reloaded.write_prb(!CIAB_DSKMOTOR & !CIAB_DSKSEL0);
        reloaded.tick(MOTOR_READY_CCK, 0, &mut []);
        reloaded.ensure_track(0, 0);
        assert_eq!(
            reloaded.drives[0].cached_words(),
            [0x1111, 0xAAAA, 0x3333, 0x4444]
        );

        let _ = fs::remove_file(path);
        Ok(())
    }

    #[test]
    fn active_write_dma_splits_output_when_head_steps_to_new_track() -> Result<()> {
        let track0 = [0x0000, 0x0000];
        let track2 = [0xFFFF, 0xFFFF];
        let path = temp_ext2_raw_tracks(&[(0, &track0), (2, &track2)])?;
        let cfg = FloppyConfig {
            drives: [
                Some(FloppyDriveConfig {
                    path: path.clone(),
                    write_protected: false,
                }),
                None,
                None,
                None,
            ],
        };
        let mut ctrl = FloppyController::from_config(&cfg)?;
        let mut chip_ram = vec![0xAA, 0xAA, 0x55, 0x55];

        let selected = !CIAB_DSKMOTOR & !CIAB_DSKSEL0;
        ctrl.write_prb(selected);
        ctrl.tick(MOTOR_READY_CCK, 0, &mut chip_ram);
        ctrl.ensure_track(0, 0);
        ctrl.drives[0].set_rotation_word(0);
        ctrl.drives[0].rotation_acc_cck = 0;
        ctrl.set_dskpt_low(0);

        let len = DSKLEN_DMAEN | DSKLEN_WRITE | 2;
        assert!(!ctrl.write_dsklen(len, 0));
        assert!(!ctrl.write_dsklen(len, 0));
        let dmacon = DMACON_DMAEN | DMACON_DISK;
        let word_cck = FloppyController::word_cck_for_track_words(track0.len());
        assert!(!ctrl.tick(word_cck, dmacon, &mut chip_ram));

        let step_high = selected & !CIAB_DSKDIREC;
        ctrl.write_prb(step_high);
        ctrl.write_prb(step_high & !CIAB_DSKSTEP);
        assert_eq!(ctrl.track_for_drive(0), 2);

        assert!(ctrl.tick(word_cck, dmacon, &mut chip_ram));

        let mut reloaded = FloppyController::from_config(&cfg)?;
        reloaded.write_prb(selected);
        reloaded.tick(MOTOR_READY_CCK, 0, &mut []);
        reloaded.ensure_track(0, 0);
        assert_eq!(reloaded.drives[0].cached_words(), [0xAAAA, 0x0000]);
        reloaded.ensure_track(0, 2);
        assert_eq!(reloaded.drives[0].cached_words(), [0xFFFF, 0x5557]);

        let _ = fs::remove_file(path);
        Ok(())
    }

    #[test]
    fn writable_legacy_extended_adf_raw_track_persists_word_stream() -> Result<()> {
        let path = temp_ext1_raw(&[0x4489, 0x1111, 0x2222])?;
        let cfg = FloppyConfig {
            drives: [
                Some(FloppyDriveConfig {
                    path: path.clone(),
                    write_protected: false,
                }),
                None,
                None,
                None,
            ],
        };
        let mut ctrl = FloppyController::from_config(&cfg)?;
        let words: [u16; 3] = [0x1234, 0x5678, 0x9ABC];
        let mut chip_ram = vec![0u8; words.len() * 2 + 2];
        for (i, word) in words.iter().copied().enumerate() {
            let [hi, lo] = word.to_be_bytes();
            chip_ram[i * 2] = hi;
            chip_ram[i * 2 + 1] = lo;
        }

        ctrl.write_prb(!CIAB_DSKMOTOR & !CIAB_DSKSEL0);
        ctrl.tick(MOTOR_READY_CCK, 0, &mut chip_ram);
        ctrl.ensure_track(0, 0);
        ctrl.drives[0].set_rotation_word(0);
        ctrl.drives[0].rotation_acc_cck = 0;
        ctrl.set_dskpt_low(0);
        let len = DSKLEN_DMAEN | DSKLEN_WRITE | words.len() as u16;
        assert!(!ctrl.write_dsklen(len, 0));
        assert!(!ctrl.write_dsklen(len, 0));
        let dmacon = DMACON_DMAEN | DMACON_DISK;
        while !ctrl.tick(ctrl.word_cck(), dmacon, &mut chip_ram) {}

        let persisted = fs::read(&path)?;
        assert_eq!(&persisted[0..8], UAE_EXT1_SIGNATURE);
        assert_eq!(u16::from_be_bytes([persisted[8], persisted[9]]), 0x1234);
        assert_eq!(u16::from_be_bytes([persisted[10], persisted[11]]), 4);
        let payload_off = 8 + 160 * 4;
        assert_eq!(
            &persisted[payload_off..payload_off + 4],
            &[0x56, 0x78, 0x9A, 0xBA]
        );

        let mut reloaded = FloppyController::from_config(&cfg)?;
        reloaded.write_prb(!CIAB_DSKMOTOR & !CIAB_DSKSEL0);
        reloaded.tick(MOTOR_READY_CCK, 0, &mut []);
        reloaded.ensure_track(0, 0);
        assert_eq!(reloaded.drives[0].cached_words(), [0x1234, 0x5678, 0x9ABA]);

        let _ = fs::remove_file(path);
        Ok(())
    }

    #[test]
    fn writable_legacy_extended_adf_raw_track_overwrites_sync_boundary() -> Result<()> {
        let path = temp_ext1_raw(&[0x4489, 0x1111, 0x2222])?;
        let cfg = FloppyConfig {
            drives: [
                Some(FloppyDriveConfig {
                    path: path.clone(),
                    write_protected: false,
                }),
                None,
                None,
                None,
            ],
        };
        let mut ctrl = FloppyController::from_config(&cfg)?;
        let mut chip_ram = vec![0xAB, 0xCD];

        ctrl.write_prb(!CIAB_DSKMOTOR & !CIAB_DSKSEL0);
        ctrl.tick(MOTOR_READY_CCK, 0, &mut chip_ram);
        ctrl.ensure_track(0, 0);
        // Head at word 0, 8 cells in (bit 8) -- the write's landing bit phase.
        ctrl.drives[0].set_rotation_bit(8);
        let word_cck = FloppyController::word_cck_for_track_words(3);
        ctrl.set_dskpt_low(0);
        let len = DSKLEN_DMAEN | DSKLEN_WRITE | 1;
        assert!(!ctrl.write_dsklen(len, 0));
        assert!(!ctrl.write_dsklen(len, 0));
        let dmacon = DMACON_DMAEN | DMACON_DISK;
        while !ctrl.tick(word_cck, dmacon, &mut chip_ram) {}

        let persisted = fs::read(&path)?;
        assert_eq!(u16::from_be_bytes([persisted[8], persisted[9]]), 0x44AB);
        assert_eq!(u16::from_be_bytes([persisted[10], persisted[11]]), 4);
        let payload_off = 8 + 160 * 4;
        assert_eq!(
            &persisted[payload_off..payload_off + 4],
            &[0xC9, 0x11, 0x22, 0x22]
        );

        let mut reloaded = FloppyController::from_config(&cfg)?;
        reloaded.write_prb(!CIAB_DSKMOTOR & !CIAB_DSKSEL0);
        reloaded.tick(MOTOR_READY_CCK, 0, &mut []);
        reloaded.ensure_track(0, 0);
        assert_eq!(reloaded.drives[0].cached_words(), [0x44AB, 0xC911, 0x2222]);

        let _ = fs::remove_file(path);
        Ok(())
    }

    #[test]
    fn writable_legacy_extended_adf_raw_track_preserves_odd_payload_length() -> Result<()> {
        let path = temp_ext1_raw_payload(0x4489, &[0x12, 0x34, 0xA0])?;
        let cfg = FloppyConfig {
            drives: [
                Some(FloppyDriveConfig {
                    path: path.clone(),
                    write_protected: false,
                }),
                None,
                None,
                None,
            ],
        };
        let mut ctrl = FloppyController::from_config(&cfg)?;
        let mut chip_ram = vec![0xFF, 0xFF];

        ctrl.write_prb(!CIAB_DSKMOTOR & !CIAB_DSKSEL0);
        ctrl.tick(MOTOR_READY_CCK, 0, &mut chip_ram);
        ctrl.ensure_track(0, 0);
        ctrl.drives[0].set_rotation_word(1);
        ctrl.drives[0].rotation_acc_cck = 0;
        ctrl.set_dskpt_low(0);
        let len = DSKLEN_DMAEN | DSKLEN_WRITE | 1;
        assert!(!ctrl.write_dsklen(len, 0));
        assert!(!ctrl.write_dsklen(len, 0));
        let dmacon = DMACON_DMAEN | DMACON_DISK;
        while !ctrl.tick(ctrl.word_cck(), dmacon, &mut chip_ram) {}

        let persisted = fs::read(&path)?;
        assert_eq!(u16::from_be_bytes([persisted[8], persisted[9]]), 0x4489);
        assert_eq!(u16::from_be_bytes([persisted[10], persisted[11]]), 3);
        let payload_off = 8 + 160 * 4;
        assert_eq!(
            &persisted[payload_off..payload_off + 3],
            &[0xFF, 0xFC, 0xA0]
        );

        let mut reloaded = FloppyController::from_config(&cfg)?;
        reloaded.write_prb(!CIAB_DSKMOTOR & !CIAB_DSKSEL0);
        reloaded.tick(MOTOR_READY_CCK, 0, &mut []);
        reloaded.ensure_track(0, 0);
        assert_eq!(reloaded.drives[0].cached_words(), [0x4489, 0xFFFC, 0xA000]);

        let _ = fs::remove_file(path);
        Ok(())
    }

    #[test]
    fn write_dma_decodes_and_persists_track() -> Result<()> {
        let path = temp_adf()?;
        let cfg = FloppyConfig {
            drives: [
                Some(FloppyDriveConfig {
                    path: path.clone(),
                    write_protected: false,
                }),
                None,
                None,
                None,
            ],
        };
        let mut ctrl = FloppyController::from_config(&cfg)?;
        let mut source = vec![0u8; ADF_SIZE];
        source[0..BYTES_PER_SECTOR].fill(0xA5);
        let words = encode_adf_track(0, &source);
        let mut chip_ram = vec![0u8; words.len() * 2 + 2];
        for (i, word) in words.iter().copied().enumerate() {
            let [hi, lo] = word.to_be_bytes();
            chip_ram[i * 2] = hi;
            chip_ram[i * 2 + 1] = lo;
        }

        ctrl.write_prb(!CIAB_DSKMOTOR & !CIAB_DSKSEL0);
        ctrl.tick(MOTOR_READY_CCK, 0, &mut chip_ram);
        ctrl.set_dskpt_low(0);
        let len = DSKLEN_DMAEN | DSKLEN_WRITE | (words.len() as u16 & DSKLEN_MASK);
        assert!(!ctrl.write_dsklen(len, 0));
        assert!(!ctrl.write_dsklen(len, 0));
        let dmacon = DMACON_DMAEN | DMACON_DISK;
        while !ctrl.tick(ctrl.word_cck(), dmacon, &mut chip_ram) {}

        let persisted = fs::read(&path)?;
        assert_eq!(&persisted[0..BYTES_PER_SECTOR], &[0xA5; BYTES_PER_SECTOR]);
        let _ = fs::remove_file(path);
        Ok(())
    }

    #[test]
    fn trackloader_sized_wordsync_window_decodes_full_amigados_track() -> Result<()> {
        let mut adf = vec![0u8; ADF_SIZE];
        let track = 35;
        for sector in 0..SECTORS_PER_TRACK {
            let off = adf_sector_offset(track, sector);
            for (idx, byte) in adf[off..off + BYTES_PER_SECTOR].iter_mut().enumerate() {
                *byte = (track as u8).wrapping_mul(3) ^ (sector as u8) ^ idx as u8;
            }
        }

        let words = encode_adf_track(track, &adf);
        let sync_positions = words
            .iter()
            .enumerate()
            .filter_map(|(idx, word)| (*word == DEFAULT_DSKSYNC).then_some(idx))
            .collect::<Vec<_>>();

        for sync_pos in sync_positions {
            let mut window = Vec::with_capacity(6400);
            window.push(0);
            for offset in 1..=6398 {
                window.push(words[(sync_pos + offset) % words.len()]);
            }
            window.push(DEFAULT_DSKSYNC);
            if window[1] != DEFAULT_DSKSYNC {
                window[0] = DEFAULT_DSKSYNC;
            }

            let decoded = decode_track_write(track, &window)
                .with_context(|| format!("decoding wordsync window after sync {sync_pos}"))?;
            assert_eq!(decoded.len(), SECTORS_PER_TRACK);
            for (sector, data) in decoded {
                let off = adf_sector_offset(track, sector);
                assert_eq!(&data[..], &adf[off..off + BYTES_PER_SECTOR]);
            }
        }

        Ok(())
    }

    #[test]
    fn standard_adf_pal_revolution_leaves_index_gap_after_last_sector() -> Result<()> {
        let adf = vec![0u8; ADF_SIZE];
        let words = encode_adf_track(0, &adf);
        assert_eq!(words.len(), STANDARD_ADF_TRACK_WORDS);

        let mut sector_sync = [None; SECTORS_PER_TRACK];
        let mut pos = 0usize;
        while pos < words.len() {
            if words[pos] != DEFAULT_DSKSYNC {
                pos += 1;
                continue;
            }

            let sync_pos = pos;
            while pos < words.len() && words[pos] == DEFAULT_DSKSYNC {
                pos += 1;
            }
            let Some((info, _)) = decode_block(&words, pos, 4) else {
                continue;
            };
            if info[0] == 0xFF && info[1] == 0 && usize::from(info[2]) < SECTORS_PER_TRACK {
                sector_sync[usize::from(info[2])] = Some(sync_pos);
            }
        }

        let sector10_sync = sector_sync[10].context("sector 10 sync")?;
        let sector0_sync = sector_sync[0].context("sector 0 sync")?;
        let distance_to_sector0_second_sync =
            (sector0_sync + 1 + words.len() - sector10_sync) % words.len();

        // After decoding the sector whose AmigaDOS "sectors until gap" byte is
        // 1, some raw loaders skip a fixed 0x258-byte index gap before looking
        // for sector 0. The synthetic PAL track must leave enough physical gap
        // for that skip to land before sector 0's second sync word.
        let sector10_sync_to_even_data_end = AMIGADOS_SECTOR_MFM_WORDS - 2;
        let fixed_index_gap_skip_words = 0x258 / 2;
        let scan_restart_lead_words = 3;
        let fixed_skip_restart =
            sector10_sync_to_even_data_end + fixed_index_gap_skip_words + scan_restart_lead_words;

        assert!(
            distance_to_sector0_second_sync >= fixed_skip_restart,
            "sector 0 sync is {distance_to_sector0_second_sync} words after sector 10 sync; fixed post-gap skip restarts at {fixed_skip_restart}"
        );

        Ok(())
    }

    #[test]
    fn non_wordsync_full_track_dma_uses_recovered_disk_word_phase() -> Result<()> {
        let mut adf = vec![0u8; ADF_SIZE];
        let boot = b"DOS\0";
        adf[0..boot.len()].copy_from_slice(boot);
        for (idx, byte) in adf[4..BYTES_PER_SECTOR].iter_mut().enumerate() {
            *byte = (idx as u8).wrapping_mul(3).wrapping_add(0x19);
        }
        let path = temp_path("full-track-dma-checksum.adf");
        fs::write(&path, &adf)?;
        let cfg = FloppyConfig {
            drives: [
                Some(FloppyDriveConfig {
                    path: path.clone(),
                    write_protected: true,
                }),
                None,
                None,
                None,
            ],
        };
        let mut ctrl = FloppyController::from_config(&cfg)?;
        let mut chip_ram = vec![0u8; 7358 * 2];
        ctrl.write_prb(!CIAB_DSKMOTOR & !CIAB_DSKSEL0);
        ctrl.tick(MOTOR_READY_CCK, 0, &mut chip_ram);
        ctrl.ensure_track(0, 0);
        // Arm just before a natural 16-bit MFM word boundary. Trackdisk-style
        // full-track reads do not always use WORDSYNC, so DMA must still drain
        // Paula's recovered word phase rather than starting a new phase on the
        // CPU's DSKLEN write.
        ctrl.drives[0].set_rotation_bit(3904);
        ctrl.drives[0].rotation_acc_cck = 0;
        let lead_in = ctrl.drives[0].head_cck_for_bits(15) as u32;
        ctrl.tick(lead_in, 0, &mut chip_ram);
        assert_eq!(ctrl.drives[0].rotation_bit % 16, 15);
        ctrl.set_dskpt_low(0);

        let len = DSKLEN_DMAEN | 7358;
        assert!(!ctrl.write_dsklen(len, 0));
        assert!(!ctrl.write_dsklen(len, 0));
        let dmacon = DMACON_DMAEN | DMACON_DISK;
        while !ctrl.tick(ctrl.word_cck(), dmacon, &mut chip_ram) {}

        let sectors = decode_track_write(0, &bytes_to_words(&chip_ram))?;
        let sector0 = sectors.iter().find(|(sector, _)| *sector == 0);
        let Some((_, sector0)) = sector0 else {
            panic!(
                "full-track DMA should include sector 0; decoded sectors: {:?}",
                sectors
                    .iter()
                    .map(|(sector, _)| *sector)
                    .collect::<Vec<_>>()
            );
        };
        assert_eq!(&sector0[..], &adf[0..BYTES_PER_SECTOR]);

        let _ = fs::remove_file(path);
        Ok(())
    }

    fn temp_adf() -> Result<PathBuf> {
        let path = temp_path("test.adf");
        fs::write(&path, vec![0u8; ADF_SIZE])?;
        Ok(path)
    }

    #[test]
    fn drive_connected_and_disk_inserted_track_drive_state() -> Result<()> {
        let mut ctrl = FloppyController::default();
        // DF0 is the internal drive: always connected, starts empty.
        assert!(ctrl.drive_connected(0));
        assert!(!ctrl.disk_inserted(0));
        // DF1-DF3 are not wired up by default.
        assert!(!ctrl.drive_connected(1));
        assert!(!ctrl.drive_connected(3));
        assert!(!ctrl.drive_connected(4));

        let adf = temp_adf()?;
        ctrl.insert_disk_image(0, adf.clone(), true)?;
        assert!(ctrl.disk_inserted(0));
        ctrl.eject_disk_image(0)?;
        assert!(!ctrl.disk_inserted(0));

        // A configured external drive answers the ID protocol and shows
        // as connected.
        let mut drives: [Option<FloppyDriveConfig>; 4] = std::array::from_fn(|_| None);
        drives[1] = Some(FloppyDriveConfig {
            path: adf.clone(),
            write_protected: true,
        });
        let ctrl = FloppyController::from_config(&FloppyConfig { drives })?;
        assert!(ctrl.drive_connected(1));
        assert!(ctrl.disk_inserted(1));
        let _ = fs::remove_file(&adf);
        Ok(())
    }

    fn temp_adz(adf: &[u8]) -> Result<PathBuf> {
        temp_gzip("test.adz", adf)
    }

    fn temp_gzip(name: &str, data: &[u8]) -> Result<PathBuf> {
        let path = temp_path(name);
        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(data)?;
        fs::write(&path, encoder.finish()?)?;
        Ok(path)
    }

    fn temp_ext2_raw(words: &[u16]) -> Result<PathBuf> {
        let payload: Vec<u8> = words.iter().flat_map(|word| word.to_be_bytes()).collect();
        temp_ext2_track(1, (payload.len() * 8) as u32, &payload)
    }

    fn temp_ext2_raw_tracks(raw_tracks: &[(usize, &[u16])]) -> Result<PathBuf> {
        let path = temp_path("test.ext.adf");
        let track_count = raw_tracks
            .iter()
            .map(|(track, _)| track + 1)
            .max()
            .unwrap_or(0);
        ensure!(track_count > 0, "raw track map must not be empty");
        let mut tracks = vec![None; track_count];
        for &(track, words) in raw_tracks {
            ensure!(track < track_count, "raw track index is outside track map");
            tracks[track] = Some(words);
        }

        let mut image = Vec::new();
        let mut payloads = Vec::new();
        image.extend_from_slice(UAE_EXT2_SIGNATURE);
        image.extend_from_slice(&0u16.to_be_bytes());
        image.extend_from_slice(&(track_count as u16).to_be_bytes());
        for track in tracks {
            match track {
                Some(words) => {
                    let payload: Vec<u8> =
                        words.iter().copied().flat_map(u16::to_be_bytes).collect();
                    image.extend_from_slice(&0u16.to_be_bytes());
                    image.push(0);
                    image.push(1);
                    image.extend_from_slice(&(payload.len() as u32).to_be_bytes());
                    image.extend_from_slice(&((payload.len() * 8) as u32).to_be_bytes());
                    payloads.extend_from_slice(&payload);
                }
                None => {
                    image.extend_from_slice(&[0; 12]);
                }
            }
        }
        image.extend_from_slice(&payloads);
        fs::write(&path, image)?;
        Ok(path)
    }

    fn temp_ext1_raw(words: &[u16]) -> Result<PathBuf> {
        let sync = words.first().copied().unwrap_or(DEFAULT_DSKSYNC);
        let payload: Vec<u8> = words
            .iter()
            .copied()
            .skip(1)
            .flat_map(u16::to_be_bytes)
            .collect();
        temp_ext1_raw_payload(sync, &payload)
    }

    fn temp_ext1_raw_payload(sync: u16, payload: &[u8]) -> Result<PathBuf> {
        let path = temp_path("test-legacy.ext.adf");
        let mut image = Vec::new();
        image.extend_from_slice(UAE_EXT1_SIGNATURE);
        image.extend_from_slice(&sync.to_be_bytes());
        image.extend_from_slice(&(payload.len() as u16).to_be_bytes());
        for _ in 1..160 {
            image.extend_from_slice(&0u16.to_be_bytes());
            image.extend_from_slice(&0u16.to_be_bytes());
        }
        image.extend_from_slice(payload);
        fs::write(&path, image)?;
        Ok(path)
    }

    fn temp_ext2_raw_revolutions(words: &[u16], bit_len: u32, revolutions: u8) -> Result<PathBuf> {
        let payload: Vec<u8> = words.iter().copied().flat_map(u16::to_be_bytes).collect();
        temp_ext2_track_with_revolutions(1, bit_len, revolutions, &payload)
    }

    fn temp_ext2_amigados(track_data: &[u8]) -> Result<PathBuf> {
        temp_ext2_track(0, (track_data.len() * 8) as u32, track_data)
    }

    fn temp_ext2_amigados_plus_raw(
        track_data: &[u8],
        raw_words: &[u16],
        raw_bit_len: u32,
        raw_revolutions: u8,
    ) -> Result<PathBuf> {
        let raw_payload: Vec<u8> = raw_words
            .iter()
            .copied()
            .flat_map(u16::to_be_bytes)
            .collect();
        let path = temp_path("test-mixed.ext.adf");
        let mut image = Vec::new();
        image.extend_from_slice(UAE_EXT2_SIGNATURE);
        image.extend_from_slice(&0u16.to_be_bytes());
        image.extend_from_slice(&2u16.to_be_bytes());

        image.extend_from_slice(&0u16.to_be_bytes());
        image.push(0);
        image.push(0);
        image.extend_from_slice(&(track_data.len() as u32).to_be_bytes());
        image.extend_from_slice(&((track_data.len() * 8) as u32).to_be_bytes());

        image.extend_from_slice(&0u16.to_be_bytes());
        image.push(raw_revolutions.saturating_sub(1));
        image.push(1);
        image.extend_from_slice(&(raw_payload.len() as u32).to_be_bytes());
        image.extend_from_slice(&raw_bit_len.to_be_bytes());

        image.extend_from_slice(track_data);
        image.extend_from_slice(&raw_payload);
        fs::write(&path, image)?;
        Ok(path)
    }

    fn temp_ext2_track(track_type: u8, bit_len: u32, payload: &[u8]) -> Result<PathBuf> {
        temp_ext2_track_with_revolutions(track_type, bit_len, 1, payload)
    }

    fn temp_ext2_track_with_revolutions(
        track_type: u8,
        bit_len: u32,
        revolutions: u8,
        payload: &[u8],
    ) -> Result<PathBuf> {
        let path = temp_path("test.ext.adf");
        fs::write(
            &path,
            ext2_track_image(track_type, bit_len, revolutions, payload),
        )?;
        Ok(path)
    }

    fn ext2_track_image(track_type: u8, bit_len: u32, revolutions: u8, payload: &[u8]) -> Vec<u8> {
        let mut image = Vec::new();
        image.extend_from_slice(UAE_EXT2_SIGNATURE);
        image.extend_from_slice(&0u16.to_be_bytes());
        image.extend_from_slice(&1u16.to_be_bytes());
        image.extend_from_slice(&0u16.to_be_bytes());
        image.push(revolutions.saturating_sub(1));
        image.push(track_type);
        image.extend_from_slice(&(payload.len() as u32).to_be_bytes());
        image.extend_from_slice(&bit_len.to_be_bytes());
        image.extend_from_slice(payload);
        image
    }

    fn temp_scp_raw_revolutions(revolutions: &[&[u16]], bit_len: u32) -> Result<PathBuf> {
        temp_scp_raw_revolutions_with_flags(revolutions, bit_len, SCP_FLAG_INDEX)
    }

    fn temp_scp_raw_revolutions_with_flags(
        revolutions: &[&[u16]],
        bit_len: u32,
        flags: u8,
    ) -> Result<PathBuf> {
        let rev_count = revolutions.len();
        let mut flux_payloads = Vec::with_capacity(rev_count);
        for words in revolutions {
            flux_payloads.push(scp_flux_entries_for_words(words, bit_len));
        }
        temp_scp_flux_payloads(flux_payloads, bit_len, flags)
    }

    fn temp_scp_flux_entries(entries: &[u16], bit_len: u32) -> Result<PathBuf> {
        let mut payload = Vec::with_capacity(entries.len() * 2);
        for entry in entries {
            payload.extend_from_slice(&entry.to_be_bytes());
        }
        temp_scp_flux_payloads(vec![payload], bit_len, SCP_FLAG_INDEX)
    }

    fn temp_scp_flux_payloads(
        flux_payloads: Vec<Vec<u8>>,
        bit_len: u32,
        flags: u8,
    ) -> Result<PathBuf> {
        let path = temp_path("test.scp");
        let rev_count = flux_payloads.len();
        ensure!(rev_count > 0 && rev_count <= u8::MAX as usize);
        let track_table_offset = scp_track_table_offset(flags);
        let tdh_offset = track_table_offset + SCP_TRACK_TABLE_LEN;
        let flux_offset = 4 + rev_count * 12;
        let index_time = bit_len * (AMIGA_DD_BITCELL_NS / SCP_CAPTURE_BASE_NS) as u32;

        let mut image = vec![0; tdh_offset];
        image[0..3].copy_from_slice(SCP_SIGNATURE);
        image[0x03] = 0x25;
        image[0x04] = 0x04;
        image[0x05] = rev_count as u8;
        image[0x06] = 0;
        image[0x07] = 0;
        image[0x08] = flags;
        image[0x09] = SCP_DEFAULT_16_BIT_FLUX_WIDTH;
        image[0x0A] = 0;
        image[0x0B] = 0;
        image[track_table_offset..track_table_offset + 4]
            .copy_from_slice(&(tdh_offset as u32).to_le_bytes());

        let mut track = Vec::new();
        track.extend_from_slice(b"TRK");
        track.push(0);
        let mut data_offset = flux_offset;
        for flux in &flux_payloads {
            track.extend_from_slice(&index_time.to_le_bytes());
            track.extend_from_slice(&((flux.len() / 2) as u32).to_le_bytes());
            track.extend_from_slice(&(data_offset as u32).to_le_bytes());
            data_offset += flux.len();
        }
        for flux in flux_payloads {
            track.extend_from_slice(&flux);
        }
        image.extend_from_slice(&track);
        fs::write(&path, image)?;
        Ok(path)
    }

    fn write_scp_checksum(image: &mut [u8]) {
        let checksum = scp_checksum(image);
        image[SCP_CHECKSUM_OFFSET..SCP_CHECKSUM_OFFSET + 4]
            .copy_from_slice(&checksum.to_le_bytes());
    }

    fn scp_flux_entries_for_words(words: &[u16], bit_len: u32) -> Vec<u8> {
        let ticks_per_cell = (AMIGA_DD_BITCELL_NS / SCP_CAPTURE_BASE_NS) as u16;
        let mut flux = Vec::new();
        let mut previous_transition_end = 0u32;
        for bit_idx in 0..bit_len {
            let word = words.get((bit_idx / 16) as usize).copied().unwrap_or(0);
            let bit_pos = 15 - (bit_idx % 16);
            if word & (1 << bit_pos) == 0 {
                continue;
            }
            let cells = bit_idx + 1 - previous_transition_end;
            let ticks = (cells as u16) * ticks_per_cell;
            flux.extend_from_slice(&ticks.to_be_bytes());
            previous_transition_end = bit_idx + 1;
        }
        flux
    }

    fn temp_path(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let counter = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("copperline-floppy-test-{nanos}-{counter}-{name}"))
    }
}
