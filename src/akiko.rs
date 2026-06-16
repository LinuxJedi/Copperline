// SPDX-License-Identifier: GPL-3.0-or-later

//! Akiko (CD32 gate array) at $B80000: identification, the
//! chunky-to-planar port, NVRAM lines, and the CD-ROM controller.
//!
//! Register layout, command protocol, and DMA behaviour follow WinUAE's
//! akiko.cpp, the de-facto reference for this undocumented chip:
//!
//! - $B80000.L  ID $C0CACAFE (the CD32 ROM C2P probe checks $CAFE at +2).
//! - $B80004.L  INTREQ (read-only; per-source clear rules below).
//! - $B80008.L  INTENA (only the top byte is writable).
//! - $B80010.L  data DMA base (16 sector slots of 4 KiB each).
//! - $B80014.L  misc DMA base: RX ring at +0, subcode at +$100, TX ring
//!   at +$200 (256-byte rings indexed by the inx registers).
//! - $B80018/19/1A  subcode offset / TX index / RX index (read).
//! - $B8001D/1F  TX/RX ring stop offsets (write; also clear the
//!   matching DMA-done interrupt and restart the DMA).
//! - $B80020.W  PBX: sector-slot enable mask (writes OR in; the
//!   transferred slot's bit reads back clear).
//! - $B80024.L  FLAGS (enable, PBX, TXD/RXD DMA, subcode, raw, ...).
//! - $B80028.B  PIO command/response port (unused by the ROM).
//! - $B80030/32 NVRAM I2C lines (SCL bit 7, SDA bit 6, direction
//!   register at $32) with a 24C08 EEPROM behind them.
//! - $B80038.L  C2P port (8 longwords in, 8 planar longwords out).
//!
//! The drive itself is the CD32's Chinon: commands arrive as
//! checksummed byte strings through the TX ring, responses return
//! through the RX ring. Implemented commands: noop, stop, pause,
//! unpause, multi (seek/play/read), LED, SubQ, and status/firmware.
//! Data sectors DMA into chip RAM as 2352-byte raw frames at 75 (x2
//! speed: 150) sectors per second. CD audio playback streams decoded
//! CD-DA sectors into the host mixer ring (44.1 kHz, the mixer's native
//! rate) and sends the drive's start/end notification packets.

use crate::cdrom::{CdImage, DATA_SECTOR_BYTES, LEADIN_SECTORS};
use crate::chipset::paula::CdAudioRing;

pub const AKIKO_BASE: u32 = 0x00B8_0000;
pub const AKIKO_SIZE: u32 = 0x0001_0000;

const ID: [u8; 4] = [0xC0, 0xCA, 0xCA, 0xFE];

// INTREQ/INTENA bits.
const CDINT_SUBCODE: u32 = 0x8000_0000;
const CDINT_DRIVEXMIT: u32 = 0x4000_0000;
const CDINT_DRIVERECV: u32 = 0x2000_0000;
const CDINT_RXDMADONE: u32 = 0x1000_0000;
const CDINT_TXDMADONE: u32 = 0x0800_0000;
const CDINT_PBX: u32 = 0x0400_0000;
const CDINT_OVERFLOW: u32 = 0x0200_0000;

// FLAGS bits.
const CDFLAG_SUBCODE: u32 = 0x8000_0000;
const CDFLAG_TXD: u32 = 0x4000_0000;
const CDFLAG_RXD: u32 = 0x2000_0000;
#[allow(dead_code)]
const CDFLAG_CAS: u32 = 0x1000_0000;
const CDFLAG_PBX: u32 = 0x0800_0000;
const CDFLAG_ENABLE: u32 = 0x0400_0000;
#[allow(dead_code)]
const CDFLAG_RAW: u32 = 0x0200_0000;

const CDS_PLAYING: u8 = 0x08;
const CDS_ERROR: u8 = 0x80;
const CH_ERR_CHECKSUM: u8 = 0x88;
const CH_ERR_BADCOMMAND: u8 = 0x80;
const CH_ERR_NODISK: u8 = 0xF8;

const FIRMWARE_VERSION: &[u8; 18] = b"CHINON  O-658-2 24";

/// Lengths of the drive commands (payload bytes after the command byte,
/// excluding the checksum), indexed by the low command nibble. -1 =
/// unknown command.
const COMMAND_LENGTHS: [i8; 16] = [1, 2, 1, 1, 12, 2, 1, 1, 4, 1, 2, -1, -1, -1, -1, -1];

/// Each TOC entry is returned three times in a row, like the real drive.
const TOC_REPEAT: u32 = 3;

/// Colour clocks per 1/75th second (one single-speed CD frame).
const CCK_PER_CD_FRAME: u32 = crate::chipset::paula::PAULA_CLOCK_HZ / 75;
/// TX/RX DMA restart delay: ~3 scanlines, expressed in colour clocks.
const DMA_RESTART_DELAY_CCK: u32 = 3 * 227;

fn get_long_byte(value: u32, offset: u32) -> u8 {
    (value >> (8 * (3 - offset))) as u8
}

fn put_long_byte(value: &mut u32, offset: u32, byte: u8) {
    let shift = 8 * (3 - offset);
    *value = (*value & !(0xFF << shift)) | (u32::from(byte) << shift);
}

fn to_bcd(v: u8) -> u8 {
    ((v / 10) << 4) | (v % 10)
}

fn from_bcd(v: u8) -> u32 {
    u32::from(v >> 4) * 10 + u32::from(v & 0x0F)
}

/// BCD MM:SS:FF (as found in drive commands) to a file-relative sector
/// number; negative when inside the lead-in.
fn bcd_msf_to_lsn(msf: &[u8]) -> i64 {
    let m = from_bcd(msf[0]) as i64;
    let s = from_bcd(msf[1]) as i64;
    let f = from_bcd(msf[2]) as i64;
    (m * 60 + s) * 75 + f - i64::from(LEADIN_SECTORS)
}

#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize)]
struct TocEntry {
    /// Track number 1-99, or 0xA0/0xA1/0xA2 for the session entries.
    point: u8,
    /// Q-channel control nibble (0x04 = data track).
    control: u8,
    /// File-relative start sector (meaningless for A0/A1).
    address: u32,
}

/// CD32 NVRAM: a 24C08 I2C EEPROM (1024 bytes) on Akiko's $B80030
/// lines (bit 7 = SCL, bit 6 = SDA, $B80032 = direction register).
/// Implements the I2C slave protocol: START/STOP detection, device
/// address with the block bits, word address, sequential reads and
/// page writes with ACKs. Contents persist to `path` when given.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct Nvram {
    memory: Vec<u8>,
    path: Option<std::path::PathBuf>,
    dirty: bool,

    // Line state as last seen (true = high).
    scl: bool,
    sda: bool,
    // Slave drive on SDA (true = pulling low).
    sda_drive_low: bool,

    state: I2cState,
    /// Current byte being shifted, MSB first.
    shift: u8,
    bit_count: u8,
    /// Memory address counter: block bits from the device address plus
    /// the word address byte.
    address: u16,
    /// Transaction phase after the device address byte.
    phase: I2cPhase,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
enum I2cState {
    Idle,
    /// Receiving a byte from the master (device addr, word addr, data).
    Receive,
    /// Slave ACK clock for a received byte.
    AckOut,
    /// Sending a byte to the master (read transaction).
    Send,
    /// Master ACK/NAK clock after a sent byte.
    AckIn,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
enum I2cPhase {
    DeviceAddress,
    WordAddress,
    Write,
    Read,
}

impl Nvram {
    const SIZE: usize = 1024;

    fn new(path: Option<std::path::PathBuf>) -> Self {
        let mut memory = vec![0u8; Self::SIZE];
        if let Some(path) = &path {
            match std::fs::read(path) {
                Ok(data) => {
                    let n = data.len().min(Self::SIZE);
                    memory[..n].copy_from_slice(&data[..n]);
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => log::warn!("cd32 nvram: reading {}: {e}", path.display()),
            }
        }
        Self {
            memory,
            path,
            dirty: false,
            scl: true,
            sda: true,
            sda_drive_low: false,
            state: I2cState::Idle,
            shift: 0,
            bit_count: 0,
            address: 0,
            phase: I2cPhase::DeviceAddress,
        }
    }

    /// SDA as the CPU reads it: low when either side drives it low.
    fn sda_read(&self, cpu_drives: bool, cpu_level: bool) -> bool {
        let cpu = !cpu_drives || cpu_level;
        cpu && !self.sda_drive_low
    }

    /// Feed the line levels after a register write. `scl`/`sda` are the
    /// resolved bus levels from the CPU side (input direction = high).
    fn set_lines(&mut self, scl: bool, sda: bool) {
        let scl_was = self.scl;
        let sda_was = self.sda;
        self.scl = scl;
        self.sda = sda;

        // START: SDA falls while SCL high. STOP: SDA rises while SCL high.
        if scl_was && scl {
            if sda_was && !sda {
                self.state = I2cState::Receive;
                self.phase = I2cPhase::DeviceAddress;
                self.shift = 0;
                self.bit_count = 0;
                self.sda_drive_low = false;
                return;
            }
            if !sda_was && sda {
                self.stop();
                return;
            }
        }

        if !scl_was && scl {
            // Rising clock: sample.
            match self.state {
                I2cState::Receive => {
                    self.shift = (self.shift << 1) | u8::from(sda);
                    self.bit_count += 1;
                }
                // Master NAKs (high) to end a read.
                I2cState::AckIn if sda => {
                    self.state = I2cState::Idle;
                }
                _ => {}
            }
        } else if scl_was && !scl {
            // Falling clock: change outputs.
            match self.state {
                I2cState::Receive if self.bit_count == 8 => {
                    self.byte_received();
                }
                I2cState::AckOut => {
                    self.sda_drive_low = false;
                    if self.phase == I2cPhase::Read {
                        self.load_send_byte();
                    } else {
                        self.state = I2cState::Receive;
                        self.shift = 0;
                        self.bit_count = 0;
                    }
                }
                I2cState::Send => {
                    if self.bit_count == 0 {
                        // Byte fully clocked out: release for master ACK.
                        self.sda_drive_low = false;
                        self.state = I2cState::AckIn;
                    } else {
                        self.output_next_bit();
                    }
                }
                I2cState::AckIn => {
                    // Master ACKed: continue sequential read.
                    self.address = (self.address + 1) % Self::SIZE as u16;
                    self.load_send_byte();
                }
                _ => {}
            }
        }
    }

    fn byte_received(&mut self) {
        let byte = self.shift;
        match self.phase {
            I2cPhase::DeviceAddress => {
                // 1010 xBB R/W: a 24C08 answers device code 1010 with the
                // 256-byte block index in bits 2-1.
                if byte & 0xF0 != 0xA0 {
                    self.state = I2cState::Idle;
                    return;
                }
                let block = u16::from((byte >> 1) & 0x03);
                self.address = (self.address & 0x00FF) | (block << 8);
                if byte & 1 != 0 {
                    self.phase = I2cPhase::Read;
                } else {
                    self.phase = I2cPhase::WordAddress;
                }
            }
            I2cPhase::WordAddress => {
                self.address = (self.address & 0x0300) | u16::from(byte);
                self.phase = I2cPhase::Write;
            }
            I2cPhase::Write => {
                let addr = usize::from(self.address) % Self::SIZE;
                if self.memory[addr] != byte {
                    self.memory[addr] = byte;
                    self.dirty = true;
                }
                // Page writes wrap inside the 16-byte page.
                let page = self.address & !0x000F;
                self.address = page | ((self.address + 1) & 0x000F);
            }
            I2cPhase::Read => {}
        }
        // ACK the byte: drive SDA low for the 9th clock.
        self.sda_drive_low = true;
        self.state = I2cState::AckOut;
    }

    fn load_send_byte(&mut self) {
        self.shift = self.memory[usize::from(self.address) % Self::SIZE];
        self.bit_count = 8;
        self.state = I2cState::Send;
        self.output_next_bit();
    }

    fn output_next_bit(&mut self) {
        self.bit_count -= 1;
        let bit = self.shift & (1 << self.bit_count) != 0;
        self.sda_drive_low = !bit;
    }

    fn stop(&mut self) {
        self.state = I2cState::Idle;
        self.sda_drive_low = false;
        self.phase = I2cPhase::DeviceAddress;
        if self.dirty {
            self.dirty = false;
            if let Some(path) = &self.path {
                if let Err(e) = std::fs::write(path, &self.memory) {
                    log::warn!("cd32 nvram: writing {}: {e}", path.display());
                }
            }
        }
    }
}

#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
struct C2p {
    buffer: [u32; 8],
    write_offset: usize,
    read_offset: Option<usize>,
    result: [u32; 8],
}

#[derive(serde::Serialize, serde::Deserialize)]
pub struct Akiko {
    c2p: C2p,
    nvram_lines: u8,
    nvram_direction: u8,
    nvram: Nvram,

    disc: Option<CdImage>,
    toc: Vec<TocEntry>,
    /// A disc waiting in the tray after a runtime insert: mounted (and a
    /// media-status packet volunteered) once `insert_delay_cck` of emulated
    /// time elapses, so the drive reports the absent->present transition a
    /// real tray produces instead of an instantaneous swap.
    pending_disc: Option<CdImage>,
    insert_delay_cck: i64,

    intreq: u32,
    intena: u32,
    subcodeoffset: u8,
    addressdata: u32,
    addressmisc: u32,
    subcode_address: u32,
    cdrx_address: u32,
    cdtx_address: u32,
    flags: u32,
    pbx: u32,
    cdcomtxinx: u8,
    cdcomrxinx: u8,
    cdcomtxcmp: u8,
    cdcomrxcmp: u8,
    tx_dma_delay_cck: u32,
    rx_dma_delay_cck: u32,

    command_buffer: [u8; 32],
    command_length: usize,
    command: u8,
    command_active: u8,
    checksum_error: bool,
    unknown_command: bool,

    result_buffer: [u8; 32],
    receive_length: usize,
    receive_offset: usize,
    last_rx: u8,

    /// 0 = cold, 1 = initial media status pushed, 2 = host ran INFO.
    cd_initialized: u8,
    /// A runtime insert/eject happened: volunteer a media-status packet
    /// (as the real drive does on a disc change) once the channel is idle.
    media_notify: bool,
    door: u8,
    toc_counter: i32,
    data_offset: i64,
    sector_counter: u32,
    current_sector: i64,
    seek_delay: u32,
    speed: u32,

    playing: bool,
    paused: bool,
    /// Pending audio notification: >0 counts CD frames down to a
    /// play-start packet, <0 schedules end (-1), error (-3) packets.
    audio_notify: i32,
    /// Current and one-past-end disc sectors of CD audio playback.
    play_position: i64,
    play_end: i64,
    /// Drop any buffered host CD audio on the next tick (stop command).
    flush_cd_audio: bool,
    /// Colour-clock countdown pacing audio sector production at 75 Hz.
    audio_counter_cck: i32,

    /// Colour-clock countdowns driving sector DMA and TOC pacing.
    read_counter_cck: i32,
    frame_counter_cck: i32,
    frame_sync: bool,
}

impl Default for Akiko {
    fn default() -> Self {
        Self {
            c2p: C2p::default(),
            nvram_lines: 0,
            nvram_direction: 0,
            nvram: Nvram::new(None),
            disc: None,
            toc: Vec::new(),
            pending_disc: None,
            insert_delay_cck: -1,
            intreq: 0,
            intena: 0,
            subcodeoffset: 0,
            addressdata: 0,
            addressmisc: 0,
            subcode_address: 0,
            cdrx_address: 0,
            cdtx_address: 0,
            flags: 0,
            pbx: 0,
            cdcomtxinx: 0,
            cdcomrxinx: 0,
            cdcomtxcmp: 0,
            cdcomrxcmp: 0,
            tx_dma_delay_cck: 0,
            rx_dma_delay_cck: 0,
            command_buffer: [0; 32],
            command_length: 0,
            command: 0,
            command_active: 0,
            checksum_error: false,
            unknown_command: false,
            result_buffer: [0; 32],
            receive_length: 0,
            receive_offset: 0,
            last_rx: 0,
            cd_initialized: 0,
            media_notify: false,
            door: 1,
            toc_counter: -1,
            data_offset: -1,
            sector_counter: 0,
            current_sector: -1,
            seek_delay: 0,
            speed: 1,
            playing: false,
            paused: false,
            audio_notify: 0,
            play_position: 0,
            play_end: 0,
            flush_cd_audio: false,
            audio_counter_cck: 0,
            read_counter_cck: 0,
            frame_counter_cck: 0,
            frame_sync: false,
        }
    }
}

impl Akiko {
    pub fn new() -> Self {
        Self::default()
    }

    /// Mount a CD image. The TOC is built once here. When the controller
    /// is already running (runtime disc swap, not boot-time mount), the
    /// drive volunteers a media-status packet so the OS sees the change.
    pub fn insert_disc(&mut self, disc: CdImage) {
        self.toc = build_toc(&disc);
        self.disc = Some(disc);
        self.media_notify = self.cd_initialized != 0;
    }

    /// Park a disc in the tray and mount it after `secs` of emulated tray
    /// time. Any disc currently in the drive is ejected first, so the
    /// controller volunteers a media-absent packet now and a media-present
    /// packet once the tray settles -- the absent->present transition a real
    /// drive produces, which the CD32 firmware needs to spot a disc change.
    pub fn insert_disc_after(&mut self, disc: CdImage, secs: f64) {
        self.eject_disc();
        self.pending_disc = Some(disc);
        self.insert_delay_cck =
            (secs.max(0.0) * f64::from(crate::chipset::paula::PAULA_CLOCK_HZ)) as i64;
    }

    /// Whether a disc is mounted or still waiting in the tray.
    pub fn has_disc(&self) -> bool {
        self.disc.is_some() || self.pending_disc.is_some()
    }

    /// Whether the drive is actively working: streaming CD audio, a TOC
    /// dump in progress, or a data read with the host still feeding PBX
    /// buffer slots (the same gate as run_sector_read). Feeds the
    /// status-bar CD LED.
    pub fn activity_led_on(&self) -> bool {
        (self.playing && !self.paused)
            || self.toc_counter >= 0
            || (self.data_offset >= 0
                && self.pbx != 0
                && self.flags & CDFLAG_ENABLE != 0
                && self.flags & CDFLAG_PBX != 0)
    }

    /// Remove the disc: stop playback, drop buffered audio, and volunteer
    /// a media-status packet so the OS notices the removal.
    pub fn eject_disc(&mut self) {
        // Cancel any disc still waiting in the tray from a delayed insert.
        self.pending_disc = None;
        self.insert_delay_cck = -1;
        if self.disc.take().is_none() {
            // Nothing was mounted (the OS never saw a present disc), so no
            // removal notification is owed.
            return;
        }
        self.toc.clear();
        self.playing = false;
        self.paused = false;
        self.flush_cd_audio = true;
        self.audio_notify = 0;
        self.toc_counter = -1;
        self.data_offset = -1;
        self.media_notify = self.cd_initialized != 0;
        log::info!("akiko: disc ejected");
    }

    /// System reset: clear controller state but keep the mounted disc
    /// and the NVRAM contents.
    pub fn reset(&mut self) {
        let disc = self.disc.take();
        let toc = std::mem::take(&mut self.toc);
        let path = self.nvram.path.take();
        let memory = std::mem::take(&mut self.nvram.memory);
        *self = Self::default();
        self.disc = disc;
        self.toc = toc;
        self.nvram.path = path;
        self.nvram.memory = memory;
    }

    /// Persist NVRAM to (and preload it from) `path`.
    pub fn set_nvram_path(&mut self, path: std::path::PathBuf) {
        self.nvram = Nvram::new(Some(path));
    }

    /// Resolved I2C bus levels from the CPU-side latches: an input
    /// direction floats high through the pull-ups.
    fn nvram_bus_levels(&self) -> (bool, bool) {
        let scl = self.nvram_direction & 0x80 == 0 || self.nvram_lines & 0x80 != 0;
        let sda = self.nvram_direction & 0x40 == 0 || self.nvram_lines & 0x40 != 0;
        (scl, sda)
    }

    /// The INT2 (PORTS) line into Paula, level-fed like Gayle's.
    pub fn int2_line(&self) -> bool {
        self.intreq & self.intena != 0
    }

    // ----- CPU access ------------------------------------------------------

    pub fn read(&mut self, addr: u32, size: usize, chip_ram: &mut [u8]) -> u32 {
        let offset = addr & 0xFFFF;
        if offset >= 0x8000 {
            return 0;
        }
        let mut value = 0u32;
        for i in 0..size as u32 {
            value = (value << 8) | u32::from(self.read_byte((offset + i) & 0x3F));
        }
        self.c2p_read_step(offset);
        self.run_internal(chip_ram);
        value
    }

    pub fn write(&mut self, addr: u32, size: usize, value: u32, chip_ram: &mut [u8]) {
        let offset = addr & 0xFFFF;
        if offset >= 0x8000 {
            return;
        }
        // Low byte lane first, like the hardware: the write landing on
        // byte 0 (a longword's MSB) completes a C2P entry.
        for i in (0..size as u32).rev() {
            let shift = 8 * (size as u32 - 1 - i);
            self.write_byte((offset + i) & 0x3F, (value >> shift) as u8);
        }
        self.run_internal(chip_ram);
    }

    fn read_byte(&mut self, offset: u32) -> u8 {
        match offset {
            0x00..=0x03 => ID[offset as usize],
            // INTREQ / INTENA (and the read-only INTENA mirror).
            0x04..=0x07 => get_long_byte(self.intreq, offset - 0x04),
            0x08..=0x0B => get_long_byte(self.intena, offset - 0x08),
            0x0C..=0x0F => get_long_byte(self.intena, offset - 0x0C),
            // 0x18-0x1B mirror 0x10/0x14/0x1C.
            0x10 | 0x14 | 0x18 | 0x1C => self.subcodeoffset,
            0x11 | 0x15 | 0x19 | 0x1D => self.cdcomtxinx,
            0x12 | 0x16 | 0x1A | 0x1E => self.cdcomrxinx,
            0x13 | 0x17 | 0x1B | 0x1F => 0,
            0x20 | 0x21 => get_long_byte(self.pbx, offset - 0x20 + 2),
            0x24..=0x27 => get_long_byte(self.flags, offset - 0x24),
            0x28 => {
                // PIO response port (the ROM uses RX DMA instead).
                if self.flags & CDFLAG_RXD == 0 && self.receive_offset < self.receive_length {
                    self.last_rx = self.result_buffer[self.receive_offset];
                    self.receive_offset += 1;
                    if self.receive_offset == self.receive_length {
                        self.intreq &= !CDINT_DRIVERECV;
                        self.receive_length = 0;
                        self.intreq |= CDINT_DRIVEXMIT;
                    }
                } else {
                    self.intreq &= !CDINT_DRIVERECV;
                }
                self.last_rx
            }
            0x30 => {
                let (scl, sda) = self.nvram_bus_levels();
                let sda = self.nvram.sda_read(self.nvram_direction & 0x40 != 0, sda);
                (u8::from(scl) << 7) | (u8::from(sda) << 6)
            }
            0x32 => self.nvram_direction,
            0x38..=0x3B => self.c2p_read_byte(offset),
            _ => 0,
        }
    }

    fn write_byte(&mut self, offset: u32, value: u8) {
        match offset {
            0x08..=0x0B => {
                put_long_byte(&mut self.intena, offset - 0x08, value);
                self.intena &= 0xFF00_0000;
            }
            0x10..=0x13 => {
                put_long_byte(&mut self.addressdata, offset - 0x10, value);
                self.addressdata &= 0x00FF_F000;
            }
            0x14..=0x17 => {
                put_long_byte(&mut self.addressmisc, offset - 0x14, value);
                self.addressmisc &= 0x00FF_FC00;
                self.subcode_address = self.addressmisc | 0x100;
                self.cdrx_address = self.addressmisc;
                self.cdtx_address = self.addressmisc | 0x200;
            }
            0x18 => self.intreq &= !CDINT_SUBCODE,
            0x1D => {
                self.intreq &= !CDINT_TXDMADONE;
                self.cdcomtxcmp = value;
                self.tx_dma_delay_cck = DMA_RESTART_DELAY_CCK;
            }
            0x1F => {
                self.intreq &= !CDINT_RXDMADONE;
                self.cdcomrxcmp = value;
                self.rx_dma_delay_cck = DMA_RESTART_DELAY_CCK;
            }
            0x20 | 0x21 => {
                // PBX writes OR slots in; the flag gate can hold it at 0.
                let previous = self.pbx;
                put_long_byte(&mut self.pbx, offset - 0x20 + 2, value);
                self.pbx |= previous;
                self.pbx &= 0xFFFF;
                if self.flags & CDFLAG_PBX == 0 {
                    self.pbx = 0;
                }
                self.intreq &= !CDINT_PBX;
            }
            0x24..=0x27 => {
                let previous = self.flags;
                put_long_byte(&mut self.flags, offset - 0x24, value);
                if self.flags & CDFLAG_ENABLE != 0 && previous & CDFLAG_ENABLE == 0 {
                    self.sector_counter = 0;
                    self.intreq &= !CDINT_OVERFLOW;
                }
                if self.flags & CDFLAG_PBX == 0 {
                    self.pbx = 0;
                }
                self.flags &= 0xFF80_0000;
            }
            0x28 => {
                // PIO command port (the ROM uses TX DMA instead).
                if self.flags & CDFLAG_TXD == 0 {
                    self.intreq &= !CDINT_DRIVEXMIT;
                    if self.can_send_command() {
                        self.add_command_byte(value);
                        if self.can_send_command() {
                            self.intreq |= CDINT_DRIVEXMIT;
                        }
                    }
                }
            }
            0x30 => {
                self.nvram_lines = value;
                let (scl, sda) = self.nvram_bus_levels();
                self.nvram.set_lines(scl, sda);
            }
            0x32 => {
                self.nvram_direction = value;
                let (scl, sda) = self.nvram_bus_levels();
                self.nvram.set_lines(scl, sda);
            }
            0x38..=0x3B => self.c2p_write_byte(offset, value),
            _ => {}
        }
    }

    // ----- periodic work ---------------------------------------------------

    /// Advance the controller by `cck` colour clocks: sector DMA pacing,
    /// DMA restart delays, audio playback, and status push-backs.
    pub fn tick(&mut self, cck: u32, chip_ram: &mut [u8], cd_audio: &mut CdAudioRing) {
        self.tx_dma_delay_cck = self.tx_dma_delay_cck.saturating_sub(cck);
        self.rx_dma_delay_cck = self.rx_dma_delay_cck.saturating_sub(cck);

        // Delayed disc insert: once the tray settles, mount the disc and
        // volunteer a media-status packet (present), like the real drive's
        // change notification at the end of tray motion.
        if self.pending_disc.is_some() {
            self.insert_delay_cck -= i64::from(cck);
            if self.insert_delay_cck < 0 {
                let disc = self.pending_disc.take().unwrap();
                self.toc = build_toc(&disc);
                self.disc = Some(disc);
                self.media_notify = self.cd_initialized != 0;
                log::info!("akiko: disc inserted (delayed), media-status notification queued");
            }
        }

        self.read_counter_cck -= cck as i32;
        if self.read_counter_cck <= 0 {
            self.read_counter_cck += (CCK_PER_CD_FRAME / self.speed.max(1)) as i32;
            if self.seek_delay > 0 {
                self.seek_delay -= 1;
            } else {
                self.run_sector_read(chip_ram);
            }
        }

        self.frame_counter_cck -= cck as i32;
        if self.frame_counter_cck <= 0 {
            self.frame_counter_cck += (CCK_PER_CD_FRAME / self.speed.max(1)) as i32;
            self.frame_sync = true;
        }

        if self.flush_cd_audio {
            self.flush_cd_audio = false;
            cd_audio.clear();
        }
        // CD-DA always plays at single speed: stream one decoded sector
        // into the host mixer ring per CD frame.
        self.audio_counter_cck -= cck as i32;
        if self.audio_counter_cck <= 0 {
            self.audio_counter_cck += CCK_PER_CD_FRAME as i32;
            self.stream_audio_sector(cd_audio);
        }

        self.handler();
        self.run_internal(chip_ram);
    }

    /// Produce the next CD-DA sector of the running play command.
    fn stream_audio_sector(&mut self, cd_audio: &mut CdAudioRing) {
        if !self.playing || self.paused {
            return;
        }
        if !cd_audio.wants_sector() {
            return; // mixer is behind; retry next CD frame
        }
        let Some(disc) = self.disc.as_mut() else {
            return;
        };
        if self.play_position >= self.play_end || self.play_position < 0 {
            self.playing = false;
            self.audio_notify = -1; // play end notification
            return;
        }
        let sector = self.play_position as u32;
        if disc.is_audio_sector(sector) {
            let mut raw = [0u8; crate::cdrom::RAW_SECTOR_BYTES];
            if disc.read_audio_sector(sector, &mut raw).is_ok() {
                cd_audio.push_sector(&raw);
            }
        }
        self.play_position += 1;
    }

    /// Status push-backs the drive volunteers between commands.
    fn handler(&mut self) {
        if self.receive_length != 0 {
            return;
        }
        if self.cd_initialized == 0 {
            // First status is 0x0a when booted with a CD inserted.
            if self.disc.is_some() {
                let len = self.command_media_status();
                self.start_return_data(len);
            }
            self.cd_initialized = 1;
            return;
        }
        // Runtime disc insert/eject: push the new media status as soon as
        // the receive channel is free, like the real drive's change
        // notification. WinUAE volunteers this even while a command is
        // active (cdrom_can_return_data only checks the receive length), so
        // we do not gate on command_active here -- gating on it lets the
        // boot ROM's status polling starve the notification.
        if self.media_notify {
            self.media_notify = false;
            let len = self.command_media_status();
            self.start_return_data(len);
            return;
        }
        if self.cd_initialized < 2 {
            return;
        }
        match self.audio_notify {
            n if n > 1 => self.audio_notify -= 1,
            1 => {
                // Play started.
                let len = self.playend_notify(0);
                self.start_return_data(len);
                self.audio_notify = 0;
            }
            -1 => {
                // Play finished.
                let len = self.playend_notify(1);
                self.start_return_data(len);
                self.audio_notify = 0;
            }
            -3 => {
                // Play failed (illegal address).
                let len = self.playend_notify(-1);
                self.start_return_data(len);
                self.audio_notify = 0;
            }
            _ => {}
        }
        // One TOC entry per CD frame while a TOC dump is in progress.
        if self.toc_counter >= 0 && self.command_active == 0 && self.frame_sync {
            self.frame_sync = false;
            let len = self.return_toc_entry();
            self.start_return_data(len);
        }
    }

    /// The WinUAE `akiko_internal` equivalent, run after register
    /// accesses and ticks: pump RX data out, TX commands in, and run a
    /// completed command.
    fn run_internal(&mut self, chip_ram: &mut [u8]) {
        self.return_data(chip_ram);
        self.run_command_dma(chip_ram);
        if self.command_active > 0 {
            self.command_active -= 1;
            if self.command_active == 0 {
                self.execute_command();
            }
        }
    }

    // ----- command path ----------------------------------------------------

    fn can_send_command(&self) -> bool {
        self.cd_initialized != 0 && self.command_active == 0 && self.receive_length == 0
    }

    /// TX DMA: fetch command bytes from the TX ring in chip RAM.
    fn run_command_dma(&mut self, chip_ram: &mut [u8]) {
        if self.flags & CDFLAG_TXD == 0 {
            return;
        }
        if self.flags & CDFLAG_ENABLE != 0 {
            return;
        }
        if self.cdcomtxinx == self.cdcomtxcmp {
            return;
        }
        if self.tx_dma_delay_cck > 0 {
            return;
        }
        if !self.can_send_command() {
            return;
        }
        let byte = chip_byte(chip_ram, self.cdtx_address + u32::from(self.cdcomtxinx));
        self.add_command_byte(byte);
        self.cdcomtxinx = self.cdcomtxinx.wrapping_add(1);
        if self.cdcomtxinx == self.cdcomtxcmp {
            self.intreq |= CDINT_TXDMADONE;
        }
    }

    fn add_command_byte(&mut self, byte: u8) {
        if self.command_length < self.command_buffer.len() {
            self.command_buffer[self.command_length] = byte;
        }
        self.command_length += 1;
        self.command = self.command_buffer[0];
        let cmd_len = COMMAND_LENGTHS[usize::from(self.command & 0x0F)];

        self.checksum_error = false;
        self.unknown_command = false;

        if cmd_len < 0 {
            self.unknown_command = true;
            self.command_active = 1;
            return;
        }
        let cmd_len = cmd_len as usize;
        if cmd_len + 1 > self.command_length {
            return;
        }
        let mut checksum: u8 = 0;
        for i in 0..=cmd_len {
            checksum = checksum.wrapping_add(self.command_buffer[i]);
        }
        if checksum != 0xFF {
            self.checksum_error = true;
        }
        self.command_active = 1;
        self.command_length = cmd_len;
    }

    fn execute_command(&mut self) {
        self.command_length = 0;
        self.result_buffer = [0; 32];

        if self.checksum_error || self.unknown_command {
            self.result_buffer[0] = (self.command & 0xF0) | 5;
            self.result_buffer[1] = if self.checksum_error {
                CH_ERR_CHECKSUM | self.door
            } else {
                CH_ERR_BADCOMMAND | self.door
            };
            self.start_return_data(2);
            return;
        }

        let len = match self.command & 0x0F {
            0 => {
                self.result_buffer[0] = self.command;
                1
            }
            1 => self.command_stop(),
            2 => self.command_pause(),
            3 => self.command_unpause(),
            4 => self.command_multi(),
            5 => self.command_led(),
            6 => self.command_subq(),
            7 => self.command_status(),
            _ => 0,
        };
        if len == 0 {
            self.intreq |= CDINT_DRIVEXMIT;
            return;
        }
        self.start_return_data(len);
    }

    fn check_no_disk(&mut self) -> bool {
        if self.disc.is_none() {
            self.result_buffer[1] = CH_ERR_NODISK | self.door;
            return true;
        }
        false
    }

    fn command_stop(&mut self) -> usize {
        self.audio_notify = 0;
        self.result_buffer[0] = self.command;
        if self.check_no_disk() {
            return 2;
        }
        self.result_buffer[1] = 0;
        self.stop_audio();
        2
    }

    fn command_pause(&mut self) -> usize {
        self.audio_notify = 0;
        self.toc_counter = -1;
        self.result_buffer[0] = self.command;
        if self.check_no_disk() {
            return 2;
        }
        self.result_buffer[1] = (if self.playing { CDS_PLAYING } else { 0 }) | self.door;
        if !self.paused {
            self.paused = true;
        }
        2
    }

    fn command_unpause(&mut self) -> usize {
        self.result_buffer[0] = self.command;
        if self.check_no_disk() {
            return 2;
        }
        self.result_buffer[1] = (if self.playing { CDS_PLAYING } else { 0 }) | self.door;
        self.paused = false;
        2
    }

    /// Seek / play audio / read data sectors.
    fn command_multi(&mut self) -> usize {
        let seekpos = bcd_msf_to_lsn(&self.command_buffer[1..4]);
        let endpos = bcd_msf_to_lsn(&self.command_buffer[4..7]);

        if self.playing {
            self.stop_audio();
        }
        self.paused = false;
        self.speed = if self.command_buffer[8] & 0x40 != 0 {
            2
        } else {
            1
        };
        self.result_buffer[0] = self.command;
        self.result_buffer[1] = 0;
        if self.disc.is_none() {
            self.result_buffer[1] = 1; // no disk
            return 2;
        }

        if self.command_buffer[7] & 0x80 != 0 {
            // Data read from seekpos to endpos.
            self.data_offset = seekpos;
            let distance = (self.current_sector - seekpos).unsigned_abs();
            self.seek_delay = if distance < 100 {
                1
            } else {
                ((distance / 1000) + 10).min(100) as u32
            };
            log::debug!("akiko: READ DATA {seekpos}..{endpos} speed {}x", self.speed);
            self.result_buffer[1] |= 0x02;
        } else if seekpos < 0 {
            // Play command with a lead-in address: a TOC dump.
            self.toc_counter = 0;
        } else {
            // Audio play: stream CD-DA into the host mixer from here.
            self.toc_counter = -1;
            self.result_buffer[1] = 0x42; // play starting
            self.playing = true;
            self.play_position = seekpos;
            self.play_end = endpos;
            self.audio_notify = 10; // play-start packet shortly
            log::debug!("akiko: PLAY {seekpos}..{endpos}");
        }
        2
    }

    fn command_led(&mut self) -> usize {
        let v = self.command_buffer[1];
        if v & 0x80 != 0 {
            self.result_buffer[0] = self.command;
            self.result_buffer[1] = v & 1;
            return 2;
        }
        0
    }

    fn command_subq(&mut self) -> usize {
        self.result_buffer[0] = self.command;
        self.result_buffer[1] = 0;
        // No audio position model: the 11 SubQ bytes stay zero, which
        // software reads as "no valid position yet".
        15
    }

    fn command_status(&mut self) -> usize {
        self.result_buffer[0] = self.command;
        self.result_buffer[1] = self.door;
        self.result_buffer[2..2 + FIRMWARE_VERSION.len()].copy_from_slice(FIRMWARE_VERSION);
        self.cd_initialized = 2;
        20
    }

    fn command_media_status(&mut self) -> usize {
        self.result_buffer[0] = 0x0A;
        self.result_buffer[1] = u8::from(self.disc.is_some());
        2
    }

    fn playend_notify(&mut self, status: i32) -> usize {
        self.result_buffer[0] = 4;
        self.result_buffer[1] = match status {
            s if s < 0 => CDS_ERROR, // error
            0 => CDS_PLAYING | 2,    // play started
            _ => 0,                  // play ended
        } | self.door;
        2
    }

    fn return_toc_entry(&mut self) -> usize {
        self.result_buffer[0] = 6;
        if self.toc.is_empty() {
            self.result_buffer[1] = CDS_ERROR | self.door;
            self.toc_counter = -1;
            return 15;
        }
        self.result_buffer[1] = 0x0A; // matches real CD32 captures
        let index = (self.toc_counter as u32 / TOC_REPEAT) as usize;
        let entry = toc_entry_bytes(&self.toc[index]);
        self.result_buffer[2..15].copy_from_slice(&entry);
        // Fake the head's running position, as the real firmware does.
        let counter = self.toc_counter as u32;
        self.result_buffer[6] = to_bcd(99);
        self.result_buffer[7] = to_bcd((24 + counter / 75) as u8);
        self.result_buffer[8] = to_bcd((counter % 75) as u8);
        self.toc_counter += 1;
        if (self.toc_counter as u32 / TOC_REPEAT) as usize >= self.toc.len() {
            self.toc_counter = -1;
        }
        15
    }

    fn stop_audio(&mut self) {
        self.playing = false;
        self.paused = false;
        self.play_position = 0;
        self.play_end = 0;
        self.flush_cd_audio = true;
    }

    // ----- response path ---------------------------------------------------

    fn start_return_data(&mut self, len: usize) -> bool {
        if self.receive_length > 0 || len == 0 {
            return false;
        }
        self.receive_length = len;
        let mut checksum: u8 = 0xFF;
        for i in 0..len {
            checksum = checksum.wrapping_sub(self.result_buffer[i]);
        }
        self.result_buffer[self.receive_length] = checksum;
        self.receive_length += 1;
        self.receive_offset = 0;
        self.intreq |= CDINT_DRIVERECV;
        true
    }

    /// RX DMA: write pending response bytes into the RX ring.
    fn return_data(&mut self, chip_ram: &mut [u8]) {
        if self.receive_length == 0 {
            return;
        }
        if self.flags & CDFLAG_RXD == 0 {
            return;
        }
        if self.cdcomrxinx == self.cdcomrxcmp {
            return;
        }
        if self.rx_dma_delay_cck > 0 {
            return;
        }
        while self.receive_offset < self.receive_length {
            self.last_rx = self.result_buffer[self.receive_offset];
            chip_put_byte(
                chip_ram,
                self.cdrx_address + u32::from(self.cdcomrxinx),
                self.last_rx,
            );
            self.cdcomrxinx = self.cdcomrxinx.wrapping_add(1);
            self.receive_offset += 1;
            if self.cdcomrxinx == self.cdcomrxcmp {
                self.intreq |= CDINT_RXDMADONE;
                break;
            }
        }
        if self.receive_offset == self.receive_length {
            self.receive_length = 0;
            self.receive_offset = 0;
            self.intreq &= !CDINT_DRIVERECV;
            self.intreq |= CDINT_DRIVEXMIT;
        }
    }

    // ----- sector DMA ------------------------------------------------------

    fn run_sector_read(&mut self, chip_ram: &mut [u8]) {
        if self.flags & CDFLAG_ENABLE == 0 {
            return;
        }
        if self.pbx == 0 || self.flags & CDFLAG_PBX == 0 {
            return;
        }
        if self.data_offset < 0 {
            return;
        }
        let Some(disc) = self.disc.as_mut() else {
            return;
        };
        // Use the highest available slot (Lotus Trilogy depends on it).
        let slot = (15 - self.pbx.leading_zeros().saturating_sub(16)) & 15;
        let slot = if self.pbx & (1 << slot) != 0 {
            slot
        } else {
            return;
        };
        let sector = self.data_offset + i64::from(self.sector_counter);
        self.current_sector = sector;
        if sector < 0 || !sector_in_data_track(&self.toc, sector as u32) {
            return;
        }
        let mut data = [0u8; DATA_SECTOR_BYTES];
        if disc.read_data_sector(sector as u32, &mut data).is_err() {
            return;
        }

        // Build the raw 2352-byte frame: sync + BCD MSF header + data.
        // The first four bytes carry the transfer tag the ROM expects.
        let mut raw = [0u8; 2352];
        raw[1..11].fill(0xFF);
        let msf = sector as u32 + LEADIN_SECTORS;
        raw[12] = to_bcd((msf / (60 * 75)) as u8);
        raw[13] = to_bcd(((msf / 75) % 60) as u8);
        raw[14] = to_bcd((msf % 75) as u8);
        raw[15] = 1; // mode 1
        raw[16..16 + DATA_SECTOR_BYTES].copy_from_slice(&data);
        raw[0] = 0;
        raw[1] = 0;
        raw[2] = 0;
        raw[3] = (self.sector_counter & 31) as u8;

        let base = self.addressdata + slot * 4096;
        for (i, byte) in raw.iter().enumerate() {
            chip_put_byte(chip_ram, base + i as u32, *byte);
        }
        // Clear the slot's subcode area.
        for i in 0..73 * 2 {
            chip_put_byte(chip_ram, base + 0xC00 + i, 0);
        }
        self.pbx &= !(1 << slot);
        self.intreq |= CDINT_PBX;

        if self.flags & CDFLAG_SUBCODE != 0 {
            // Sector-synchronous subcode delivery: zeroed payload with
            // the hardware's end markers.
            self.subcodeoffset = if self.subcodeoffset >= 128 { 0 } else { 128 };
            for i in 0..96 {
                chip_put_byte(
                    chip_ram,
                    self.subcode_address + u32::from(self.subcodeoffset) + i,
                    0,
                );
            }
            let tail = self.subcode_address + u32::from(self.subcodeoffset) + 96;
            chip_put_byte(chip_ram, tail, 0xFF);
            chip_put_byte(chip_ram, tail + 1, 0xFF);
            chip_put_byte(chip_ram, tail + 2, 0);
            chip_put_byte(chip_ram, tail + 3, 0);
            self.subcodeoffset = self.subcodeoffset.wrapping_add(100);
            self.intreq |= CDINT_SUBCODE;
        }

        self.sector_counter += 1;
    }

    // ----- C2P -------------------------------------------------------------

    fn c2p_read_byte(&mut self, offset: u32) -> u8 {
        let read_offset = match self.c2p.read_offset {
            Some(off) => off,
            None => {
                self.c2p_convert();
                self.c2p.write_offset = 0;
                self.c2p.read_offset = Some(0);
                0
            }
        };
        let long = self.c2p.result[read_offset];
        (long >> (8 * (3 - (offset - 0x38)))) as u8
    }

    fn c2p_write_byte(&mut self, offset: u32, value: u8) {
        let byte = (offset - 0x38) as usize;
        if byte == 3 {
            self.c2p.buffer[self.c2p.write_offset] = 0;
        }
        self.c2p.buffer[self.c2p.write_offset] |= u32::from(value) << (8 * (3 - byte));
        if byte == 0 {
            self.c2p.write_offset = (self.c2p.write_offset + 1) & 7;
        }
        self.c2p.read_offset = None;
    }

    fn c2p_read_step(&mut self, offset: u32) {
        if !(0x38..0x3C).contains(&(offset & 0x3F)) {
            return;
        }
        if let Some(read_offset) = self.c2p.read_offset.as_mut() {
            *read_offset = (*read_offset + 1) & 7;
        }
    }

    /// The C2P transpose: 8 longwords in (32 chunky 8-bit pixels, low
    /// byte of the most recent longword first), 8 planar longwords out.
    /// Bit mapping per WinUAE's reference implementation.
    fn c2p_convert(&mut self) {
        self.c2p.result = [0; 8];
        for i in 0..(8 * 32) {
            if self.c2p.buffer[7 - (i >> 5)] & (1u32 << (i & 31)) != 0 {
                self.c2p.result[i & 7] |= 1 << (i >> 3);
            }
        }
    }
}

fn chip_byte(chip_ram: &[u8], addr: u32) -> u8 {
    let len = chip_ram.len();
    if len == 0 {
        return 0;
    }
    chip_ram[(addr as usize) & (len - 1)]
}

fn chip_put_byte(chip_ram: &mut [u8], addr: u32, value: u8) {
    let len = chip_ram.len();
    if len == 0 {
        return;
    }
    chip_ram[(addr as usize) & (len - 1)] = value;
}

fn build_toc(disc: &CdImage) -> Vec<TocEntry> {
    let tracks = disc.tracks();
    let mut toc = Vec::with_capacity(tracks.len() + 3);
    let first = tracks.first().map(|t| t.number).unwrap_or(1);
    let last = tracks.last().map(|t| t.number).unwrap_or(1);
    // Session entries: first track, last track, lead-out address.
    toc.push(TocEntry {
        point: 0xA0,
        control: 0,
        address: u32::from(first),
    });
    toc.push(TocEntry {
        point: 0xA1,
        control: 0,
        address: u32::from(last),
    });
    toc.push(TocEntry {
        point: 0xA2,
        control: 0,
        address: disc.total_sectors(),
    });
    for track in tracks {
        toc.push(TocEntry {
            point: track.number,
            control: if track.kind.is_data() { 0x04 } else { 0x00 },
            address: track.start_sector,
        });
    }
    toc
}

/// One 13-byte TOC packet body, as the Chinon firmware formats it.
fn toc_entry_bytes(entry: &TocEntry) -> [u8; 13] {
    let mut d = [0u8; 13];
    d[1] = 0x01 | (entry.control << 4); // ADR 1 | control
    d[3] = if entry.point < 100 {
        to_bcd(entry.point)
    } else {
        entry.point
    };
    if entry.point == 0xA0 || entry.point == 0xA1 {
        d[8] = to_bcd(entry.address as u8);
    } else {
        let msf = entry.address + LEADIN_SECTORS;
        d[8] = to_bcd((msf / (60 * 75)) as u8);
        d[9] = to_bcd(((msf / 75) % 60) as u8);
        d[10] = to_bcd((msf % 75) as u8);
    }
    d
}

fn sector_in_data_track(toc: &[TocEntry], sector: u32) -> bool {
    // Track entries follow the three session entries; a data sector must
    // fall inside a control-0x04 track, bounded by the next track start
    // (or the lead-out for the last track).
    for i in 3..toc.len() {
        let entry = &toc[i];
        if entry.control & 0x04 == 0 {
            continue;
        }
        let end = toc
            .get(i + 1)
            .map(|next| next.address)
            .unwrap_or_else(|| toc[2].address);
        if sector >= entry.address && sector < end {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::path::PathBuf;

    fn no_chip() -> Vec<u8> {
        vec![0u8; 1024]
    }

    #[test]
    fn activity_led_tracks_audio_toc_and_data_work() {
        let mut akiko = Akiko::new();
        akiko.insert_disc(test_disc());
        assert!(!akiko.activity_led_on());

        akiko.playing = true;
        assert!(akiko.activity_led_on());
        akiko.paused = true;
        assert!(!akiko.activity_led_on());
        akiko.playing = false;
        akiko.paused = false;

        akiko.toc_counter = 0;
        assert!(akiko.activity_led_on());
        akiko.toc_counter = -1;

        // A data read only lights the LED while the host keeps feeding
        // PBX buffer slots.
        akiko.data_offset = 100;
        assert!(!akiko.activity_led_on());
        akiko.pbx = 1;
        akiko.flags = CDFLAG_ENABLE | CDFLAG_PBX;
        assert!(akiko.activity_led_on());
        akiko.pbx = 0;
        assert!(!akiko.activity_led_on());
    }

    #[test]
    fn runtime_disc_change_volunteers_media_status() {
        let mut akiko = Akiko::new();
        // Boot-time mount: the cold-start status push covers it, no
        // extra notification.
        akiko.insert_disc(test_disc());
        assert!(!akiko.media_notify);
        assert!(akiko.has_disc());

        // Runtime eject once the host is up: the drive volunteers a
        // media-status packet showing no disc.
        akiko.cd_initialized = 2;
        akiko.eject_disc();
        assert!(!akiko.has_disc());
        assert!(akiko.media_notify);
        akiko.handler();
        assert_eq!(akiko.result_buffer[0], 0x0A);
        assert_eq!(akiko.result_buffer[1], 0);
        assert!(!akiko.media_notify);
        assert!(akiko.receive_length > 0);

        // Runtime insert: another packet, now showing media present.
        akiko.receive_length = 0;
        akiko.insert_disc(test_disc());
        assert!(akiko.media_notify);
        akiko.handler();
        assert_eq!(akiko.result_buffer[0], 0x0A);
        assert_eq!(akiko.result_buffer[1], 1);
    }

    fn test_disc() -> CdImage {
        static UNIQUE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let unique = UNIQUE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let pid = std::process::id();
        let dir = std::env::temp_dir();
        let cue: PathBuf = dir.join(format!("copperline-akiko-{pid}-{unique}.cue"));
        let bin: PathBuf = dir.join(format!("copperline-akiko-{pid}-{unique}.bin"));
        let mut bytes = Vec::new();
        for s in 0u8..8 {
            bytes.extend(std::iter::repeat_n(s, DATA_SECTOR_BYTES));
        }
        let mut f = std::fs::File::create(&bin).unwrap();
        f.write_all(&bytes).unwrap();
        std::fs::write(
            &cue,
            format!(
                "FILE \"{}\" BINARY\n  TRACK 01 MODE1/2048\n    INDEX 01 00:00:00\n",
                bin.file_name().unwrap().to_string_lossy()
            ),
        )
        .unwrap();
        let image = CdImage::load(&cue).unwrap();
        let _ = std::fs::remove_file(&cue);
        image
    }

    #[test]
    fn kickstart_probe_reads_cafe_at_b80002() {
        let mut chip = no_chip();
        let mut akiko = Akiko::new();
        assert_eq!(akiko.read(AKIKO_BASE + 2, 2, &mut chip), 0xCAFE);
        assert_eq!(akiko.read(AKIKO_BASE, 4, &mut chip), 0xC0CA_CAFE);
    }

    #[test]
    fn c2p_converts_last_longword_low_byte_into_plane_bit_zero() {
        let mut chip = no_chip();
        let mut akiko = Akiko::new();
        for _ in 0..7 {
            akiko.write(AKIKO_BASE + 0x38, 4, 0, &mut chip);
        }
        akiko.write(AKIKO_BASE + 0x38, 4, 0x0000_00FF, &mut chip);
        for plane in 0..8 {
            let v = akiko.read(AKIKO_BASE + 0x38, 4, &mut chip);
            assert_eq!(v, 1, "plane {plane}");
        }
    }

    #[test]
    fn c2p_distributes_pixel_colour_bits_across_planes() {
        let mut chip = no_chip();
        let mut akiko = Akiko::new();
        for _ in 0..7 {
            akiko.write(AKIKO_BASE + 0x38, 4, 0, &mut chip);
        }
        akiko.write(AKIKO_BASE + 0x38, 4, 0x0000_0005, &mut chip);
        let plane0 = akiko.read(AKIKO_BASE + 0x38, 4, &mut chip);
        let plane1 = akiko.read(AKIKO_BASE + 0x38, 4, &mut chip);
        let plane2 = akiko.read(AKIKO_BASE + 0x38, 4, &mut chip);
        let plane3 = akiko.read(AKIKO_BASE + 0x38, 4, &mut chip);
        assert_eq!((plane0, plane1, plane2, plane3), (1, 0, 1, 0));
    }

    /// Bit-bang I2C master helpers driving the NVRAM lines through the
    /// Akiko registers, as the CD32 ROM does.
    mod i2c {
        use super::*;

        const SCL: u32 = 0x80;
        const SDA: u32 = 0x40;

        fn set(akiko: &mut Akiko, chip: &mut [u8], dir: u32, lines: u32) {
            akiko.write(AKIKO_BASE + 0x32, 1, dir, chip);
            akiko.write(AKIKO_BASE + 0x30, 1, lines, chip);
        }

        pub fn start(akiko: &mut Akiko, chip: &mut [u8]) {
            set(akiko, chip, SCL | SDA, SCL | SDA);
            set(akiko, chip, SCL | SDA, SCL); // SDA falls, SCL high
            set(akiko, chip, SCL | SDA, 0); // SCL low
        }

        pub fn stop(akiko: &mut Akiko, chip: &mut [u8]) {
            set(akiko, chip, SCL | SDA, 0);
            set(akiko, chip, SCL | SDA, SCL); // SCL high, SDA low
            set(akiko, chip, SCL | SDA, SCL | SDA); // SDA rises
        }

        /// Write one byte MSB-first and return the ACK level (false =
        /// ACKed).
        pub fn write_byte(akiko: &mut Akiko, chip: &mut [u8], byte: u8) -> bool {
            for bit in (0..8).rev() {
                let sda = if byte & (1 << bit) != 0 { SDA } else { 0 };
                set(akiko, chip, SCL | SDA, sda);
                set(akiko, chip, SCL | SDA, SCL | sda);
                set(akiko, chip, SCL | SDA, sda);
            }
            // ACK clock with SDA as input.
            set(akiko, chip, SCL, 0);
            set(akiko, chip, SCL, SCL);
            let ack = akiko.read(AKIKO_BASE + 0x30, 1, chip) & SDA != 0;
            set(akiko, chip, SCL, 0);
            ack
        }

        /// Read one byte MSB-first; `ack` = master ACKs (continue).
        pub fn read_byte(akiko: &mut Akiko, chip: &mut [u8], ack: bool) -> u8 {
            let mut byte = 0u8;
            for _ in 0..8 {
                set(akiko, chip, SCL, SCL);
                byte = (byte << 1) | u8::from(akiko.read(AKIKO_BASE + 0x30, 1, chip) & SDA != 0);
                set(akiko, chip, SCL, 0);
            }
            let sda = if ack { 0 } else { SDA };
            set(akiko, chip, SCL | SDA, sda);
            set(akiko, chip, SCL | SDA, SCL | sda);
            set(akiko, chip, SCL | SDA, sda);
            byte
        }
    }

    #[test]
    fn nvram_eeprom_round_trips_a_page_write_and_random_read() {
        let mut chip = no_chip();
        let mut akiko = Akiko::new();

        // Page write: device 1010 block1 W, word address 0x42, two bytes.
        i2c::start(&mut akiko, &mut chip);
        assert!(!i2c::write_byte(&mut akiko, &mut chip, 0xA2), "addr ACK");
        assert!(!i2c::write_byte(&mut akiko, &mut chip, 0x42), "word ACK");
        assert!(!i2c::write_byte(&mut akiko, &mut chip, 0xDE), "data ACK");
        assert!(!i2c::write_byte(&mut akiko, &mut chip, 0xAD), "data ACK");
        i2c::stop(&mut akiko, &mut chip);
        assert_eq!(akiko.nvram.memory[0x142], 0xDE);
        assert_eq!(akiko.nvram.memory[0x143], 0xAD);

        // Random read: set the address with a write header, repeated
        // START, then read two bytes sequentially.
        i2c::start(&mut akiko, &mut chip);
        assert!(!i2c::write_byte(&mut akiko, &mut chip, 0xA2));
        assert!(!i2c::write_byte(&mut akiko, &mut chip, 0x42));
        i2c::start(&mut akiko, &mut chip); // repeated start
        assert!(
            !i2c::write_byte(&mut akiko, &mut chip, 0xA3),
            "read addr ACK"
        );
        assert_eq!(i2c::read_byte(&mut akiko, &mut chip, true), 0xDE);
        assert_eq!(i2c::read_byte(&mut akiko, &mut chip, false), 0xAD);
        i2c::stop(&mut akiko, &mut chip);

        // A non-EEPROM device address is ignored (no ACK).
        i2c::start(&mut akiko, &mut chip);
        assert!(i2c::write_byte(&mut akiko, &mut chip, 0x55), "no ACK");
        i2c::stop(&mut akiko, &mut chip);
    }

    #[test]
    fn audio_play_streams_sectors_into_mixer_ring_and_notifies_end() {
        // Disc: 2 data sectors then 4 audio sectors of a known sample.
        let nanos_unique = {
            static UNIQUE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            UNIQUE.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        };
        let dir = std::env::temp_dir();
        let cue = dir.join(format!(
            "copperline-akiko-audio-{}-{nanos_unique}.cue",
            std::process::id()
        ));
        let bin = dir.join(format!(
            "copperline-akiko-audio-{}-{nanos_unique}.bin",
            std::process::id()
        ));
        let mut bytes = vec![0u8; 2 * DATA_SECTOR_BYTES];
        // Audio frames: left = 0x1000, right = 0x2000 (little endian).
        for _ in 0..4 * (crate::cdrom::RAW_SECTOR_BYTES / 4) {
            bytes.extend_from_slice(&[0x00, 0x10, 0x00, 0x20]);
        }
        std::fs::write(
            &cue,
            format!(
                concat!(
                    "FILE \"{}\" BINARY\n",
                    "  TRACK 01 MODE1/2048\n    INDEX 01 00:00:00\n",
                    "  TRACK 02 AUDIO\n    INDEX 01 00:00:02\n",
                ),
                bin.file_name().unwrap().to_string_lossy()
            ),
        )
        .unwrap();
        std::fs::write(&bin, &bytes).unwrap();
        let image = CdImage::load(&cue).unwrap();
        let _ = std::fs::remove_file(&cue);
        let _ = std::fs::remove_file(&bin);

        let mut chip = vec![0u8; 64 * 1024];
        let mut ring = CdAudioRing::default();
        let mut akiko = Akiko::new();
        akiko.insert_disc(image);
        akiko.tick(2048, &mut chip, &mut ring);
        akiko.cd_initialized = 2;
        akiko.receive_length = 0;

        // PLAY track 2: MSF 00:02:02 (disc sector 2) to 00:02:06.
        let response = dma_command(
            &mut akiko,
            &mut chip,
            &[
                0x04, 0x00, 0x02, 0x02, 0x00, 0x02, 0x06, 0x00, 0x00, 0x00, 0x00, 0x00,
            ],
        );
        assert_eq!(response[0], 0x04);
        assert_eq!(response[1] & 0x42, 0x42, "play-start status");
        assert!(akiko.playing);

        // Stream the 4 sectors (75 Hz pacing) plus the end notification.
        akiko.write(
            AKIKO_BASE + 0x1F,
            1,
            u32::from(akiko.cdcomrxinx.wrapping_sub(1)),
            &mut chip,
        );
        for _ in 0..16 {
            akiko.tick(CCK_PER_CD_FRAME / 2, &mut chip, &mut ring);
        }
        assert!(!akiko.playing, "play should have reached the end");
        let (left, right) = ring.next_sample();
        assert!((left - 0x1000 as f32 / 32768.0).abs() < 1e-4);
        assert!((right - 0x2000 as f32 / 32768.0).abs() < 1e-4);

        // The play-end packet (type 4, CDS_PLAYEND) lands in the RX ring.
        let rx_base = 0x1000usize;
        let ring_bytes = &chip[rx_base..rx_base + 0x100];
        let found = (0..0x100).any(|i| ring_bytes[i] == 4 && ring_bytes[(i + 1) & 0xFF] == 0x01);
        assert!(found, "play-end notification not found in RX ring");
    }

    /// Run the full DMA command/response round trip the CD32 ROM uses:
    /// rings at $1000, command written to the TX ring, response read
    /// back from the RX ring.
    fn dma_command(akiko: &mut Akiko, chip: &mut [u8], cmd: &[u8]) -> Vec<u8> {
        let mut ring = CdAudioRing::default();
        const MISC: u32 = 0x0000_1000; // addressmisc base
        akiko.write(AKIKO_BASE + 0x14, 4, MISC, chip);
        // Enable TX/RX DMA in flags.
        akiko.write(AKIKO_BASE + 0x24, 4, CDFLAG_TXD | CDFLAG_RXD, chip);

        // Write the command (with checksum) into the TX ring.
        let tx_base = (MISC | 0x200) as usize;
        let start = akiko.cdcomtxinx;
        let mut checksum = 0xFFu8;
        for (i, b) in cmd.iter().enumerate() {
            chip[tx_base + ((start as usize + i) & 0xFF)] = *b;
            checksum = checksum.wrapping_sub(*b);
        }
        chip[tx_base + ((start as usize + cmd.len()) & 0xFF)] = checksum;
        let end = start.wrapping_add(cmd.len() as u8 + 1);
        // Arm RX for a full ring, then kick TX.
        akiko.write(
            AKIKO_BASE + 0x1F,
            1,
            u32::from(akiko.cdcomrxinx.wrapping_sub(1)),
            chip,
        );
        akiko.write(AKIKO_BASE + 0x1D, 1, u32::from(end), chip);

        // Let the DMA delays elapse and the command run.
        let rx_start = akiko.cdcomrxinx;
        for _ in 0..64 {
            akiko.tick(2048, chip, &mut ring);
        }
        let rx_end = akiko.cdcomrxinx;
        let rx_base = MISC as usize;
        let mut out = Vec::new();
        let mut i = rx_start;
        while i != rx_end {
            out.push(chip[rx_base + i as usize]);
            i = i.wrapping_add(1);
        }
        out
    }

    #[test]
    fn status_command_returns_firmware_string_with_checksum() {
        let mut ring = CdAudioRing::default();
        let mut chip = vec![0u8; 64 * 1024];
        let mut akiko = Akiko::new();
        akiko.insert_disc(test_disc());
        // Boot push: media status packet goes out first.
        akiko.tick(2048, &mut chip, &mut ring);
        akiko.cd_initialized = 2;
        akiko.receive_length = 0;

        let response = dma_command(&mut akiko, &mut chip, &[0x17]);
        assert!(response.len() >= 21, "short response: {response:02X?}");
        assert_eq!(response[0], 0x17);
        assert_eq!(&response[2..20], FIRMWARE_VERSION);
        let sum: u8 = response.iter().fold(0u8, |acc, b| acc.wrapping_add(*b));
        assert_eq!(sum, 0xFF, "response checksum invalid: {response:02X?}");
    }

    #[test]
    fn checksum_error_returns_error_packet() {
        let mut ring = CdAudioRing::default();
        let mut chip = vec![0u8; 64 * 1024];
        let mut akiko = Akiko::new();
        akiko.insert_disc(test_disc());
        akiko.tick(2048, &mut chip, &mut ring);
        akiko.cd_initialized = 2;
        akiko.receive_length = 0;

        // Corrupt the checksum by sending command 0x17 with a wrong
        // trailing byte: build manually.
        const MISC: u32 = 0x0000_1000;
        akiko.write(AKIKO_BASE + 0x14, 4, MISC, &mut chip);
        akiko.write(AKIKO_BASE + 0x24, 4, CDFLAG_TXD | CDFLAG_RXD, &mut chip);
        let tx_base = (MISC | 0x200) as usize;
        let start = akiko.cdcomtxinx as usize;
        chip[tx_base + start] = 0x17;
        chip[tx_base + start + 1] = 0x00;
        chip[tx_base + start + 2] = 0x12; // bad checksum
        akiko.write(
            AKIKO_BASE + 0x1F,
            1,
            u32::from(akiko.cdcomrxinx.wrapping_sub(1)),
            &mut chip,
        );
        akiko.write(AKIKO_BASE + 0x1D, 1, (start as u32 + 3) & 0xFF, &mut chip);
        let rx_start = akiko.cdcomrxinx;
        for _ in 0..64 {
            akiko.tick(2048, &mut chip, &mut ring);
        }
        let rx_base = MISC as usize;
        assert_eq!(chip[rx_base + rx_start as usize], 0x15); // cmd|5 error tag
        assert_eq!(
            chip[rx_base + rx_start as usize + 1] & 0xF8,
            CH_ERR_CHECKSUM
        );
    }

    #[test]
    fn data_read_command_dmas_sectors_into_pbx_slots() {
        let mut ring = CdAudioRing::default();
        let mut chip = vec![0u8; 256 * 1024];
        let mut akiko = Akiko::new();
        akiko.insert_disc(test_disc());
        akiko.tick(2048, &mut chip, &mut ring);
        akiko.cd_initialized = 2;
        akiko.receive_length = 0;

        // READ DATA sectors 0..4 (MSF 00:02:00 - 00:02:04), double speed.
        let response = dma_command(
            &mut akiko,
            &mut chip,
            &[
                0x04, 0x00, 0x02, 0x00, 0x00, 0x02, 0x04, 0x80, 0x40, 0x00, 0x00, 0x00,
            ],
        );
        assert!(!response.is_empty());
        assert_eq!(response[0], 0x04);
        assert_eq!(response[1] & 0x02, 0x02, "data-read ack flag");

        // Point the data DMA at $10000, enable transfers, open 2 slots.
        akiko.write(AKIKO_BASE + 0x10, 4, 0x0001_0000, &mut chip);
        akiko.write(
            AKIKO_BASE + 0x24,
            4,
            CDFLAG_TXD | CDFLAG_RXD | CDFLAG_ENABLE | CDFLAG_PBX | CDFLAG_CAS,
            &mut chip,
        );
        akiko.write(AKIKO_BASE + 0x20, 2, 0x0003, &mut chip);

        // Two CD frames at 2x speed.
        for _ in 0..40 {
            akiko.tick(CCK_PER_CD_FRAME / 8, &mut chip, &mut ring);
        }

        // Highest slot first: sector 0 lands in slot 1, sector 1 in 0.
        let slot1 = 0x0001_0000 + 4096;
        assert_eq!(chip[slot1 + 3], 0); // transfer tag: first sector
        assert_eq!(chip[slot1 + 16], 0x00); // sector 0 payload byte
        let slot0 = 0x0001_0000;
        assert_eq!(chip[slot0 + 3], 1); // second sector tag
        assert_eq!(chip[slot0 + 16], 0x01); // sector 1 payload
                                            // Both slots consumed, PBX interrupt raised.
        assert_eq!(akiko.pbx, 0);
        assert_ne!(akiko.intreq & CDINT_PBX, 0);
    }

    #[test]
    fn toc_dump_streams_entries_after_leadin_play_command() {
        let mut ring = CdAudioRing::default();
        let mut chip = vec![0u8; 64 * 1024];
        let mut akiko = Akiko::new();
        akiko.insert_disc(test_disc());
        akiko.tick(2048, &mut chip, &mut ring);
        akiko.cd_initialized = 2;
        akiko.receive_length = 0;

        // Play from MSF 00:00:00 (inside the lead-in): a TOC request.
        let response = dma_command(
            &mut akiko,
            &mut chip,
            &[
                0x04, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            ],
        );
        assert_eq!(response[0], 0x04);
        assert!(akiko.toc_counter >= 0, "TOC dump should be armed");

        // Re-arm RX and let the whole TOC stream (one packet per CD
        // frame, each entry repeated three times).
        akiko.write(
            AKIKO_BASE + 0x1F,
            1,
            u32::from(akiko.cdcomrxinx.wrapping_sub(1)),
            &mut chip,
        );
        for _ in 0..400 {
            akiko.tick(CCK_PER_CD_FRAME / 4, &mut chip, &mut ring);
        }
        assert_eq!(akiko.toc_counter, -1, "TOC dump should have completed");

        // Find the lead-out (A2) packet in the RX ring: packet type 6,
        // status 0x0A, point byte A2, MSF = 8 sectors + lead-in =
        // 00:02:08 in BCD.
        let rx_base = 0x1000usize;
        let ring = &chip[rx_base..rx_base + 0x100];
        let found = (0..0x100).any(|i| {
            ring[i] == 6
                && ring[(i + 1) & 0xFF] == 0x0A
                && ring[(i + 5) & 0xFF] == 0xA2
                && ring[(i + 10) & 0xFF] == 0x00
                && ring[(i + 11) & 0xFF] == 0x02
                && ring[(i + 12) & 0xFF] == 0x08
        });
        assert!(found, "lead-out TOC packet not found in RX ring");
    }
}
