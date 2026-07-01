// SPDX-License-Identifier: GPL-3.0-or-later

//! CDTV DMAC / CD-ROM controller.
//!
//! The CDTV's CD subsystem is a Zorro II autoconfig board (Commodore
//! product 3) that the ROM maps during autoconfig (conventionally at
//! $E90000). The 64 KiB window carries, per WinUAE's cdtv.cpp (the
//! reference for this hardware):
//!
//! - +$00..$3F  the board's autoconfig ROM, still readable when mapped;
//! - +$41/$43   DMAC ISTR (interrupt status) / CNTR (control);
//! - +$80..$87  DMAC WTC (word transfer count) and ACR (address);
//! - +$A1       the Matshita drive's command/response byte port;
//! - +$B0..$BF  a 6525 TPI (tri-port interface) whose port C carries
//!   the drive handshake lines: SBCP (subcode byte), SCOR (subcode
//!   frame), STCH (status change), STEN (status byte available); port
//!   B drives CMD/ENABLE/XAEN/DTEN and the DAC volume strobes;
//! - +$E0/$E2/$E4  DMA start / stop / clear-interrupt strobes.
//!
//! Drive commands are fixed-length byte strings (no checksum): seek,
//! read, motor, play (LSN/MSF/track), status, error, model, mode
//! set/sense, capacity, SubQ, info, TOC, pause, front panel. Responses
//! return one byte at a time through +$A1, with STEN pulsing per byte.
//! Data sectors DMA onto the system bus at the ACR address, paced at
//! single speed; completion raises the DMAC E_INT interrupt. The DMAC is
//! a 24-bit bus master like the A2091's: targets can be chip RAM, slow
//! RAM, or Zorro II board RAM (the CD driver allocates CD buffers in
//! fast RAM when a board is fitted).
//!
//! CD audio streams into the shared host mixer ring like the CD32's
//! Akiko. Subcode payload delivery (CD+G) is not implemented: SCOR
//! pulses with the motor on, but SBCP never presents data.

use crate::cdrom::{to_bcd, CdImage, LEADIN_SECTORS, RAW_SECTOR_BYTES};
use crate::chipset::paula::CdAudioRing;
use crate::memory::Memory;

// DMAC CNTR bits.
const CNTR_TCEN: u8 = 1 << 7;
const CNTR_PREST: u8 = 1 << 6;
const CNTR_PDMD: u8 = 1 << 5;
const CNTR_INTEN: u8 = 1 << 4;
#[allow(dead_code)]
const CNTR_DDIR: u8 = 1 << 3;

// DMAC ISTR bits.
const ISTR_INT_P: u8 = 1 << 4;
const ISTR_E_INT: u8 = 1 << 5;
const ISTR_FE_FLG: u8 = 1 << 0;

const MODEL_NAME: &[u8] = b"MATSHITA0.96";

// CD audio status values (SCSI-style, as cdtv.device expects).
const AUDIO_STATUS_IN_PROGRESS: u8 = 0x11;
const AUDIO_STATUS_PAUSED: u8 = 0x12;
const AUDIO_STATUS_PLAY_COMPLETE: u8 = 0x13;
const AUDIO_STATUS_NOT_SUPPORTED: u8 = 0x00;
const AUDIO_STATUS_NO_STATUS: u8 = 0x15;

/// Colour clocks per scanline (the reference paces in hsyncs).
const CCK_PER_LINE: u32 = 227;
/// Colour clocks per 1/75th second (one single-speed CD frame).
const CCK_PER_CD_FRAME: u32 = crate::chipset::paula::PAULA_CLOCK_HZ / 75;

fn lsn_to_msf(lsn: u32) -> u32 {
    let msf = lsn + LEADIN_SECTORS;
    ((msf / (60 * 75)) << 16) | (((msf / 75) % 60) << 8) | (msf % 75)
}

fn msf_to_lsn(msf: u32) -> u32 {
    let m = (msf >> 16) & 0xFF;
    let s = (msf >> 8) & 0xFF;
    let f = msf & 0xFF;
    ((m * 60 + s) * 75 + f).saturating_sub(LEADIN_SECTORS)
}

#[derive(serde::Serialize, serde::Deserialize)]
pub struct CdtvController {
    /// Autoconfig: base address once the ROM configures the board.
    configured_base: Option<u32>,
    shut_up: bool,
    #[serde(with = "serde_big_array::BigArray")]
    config_rom: [u8; 0x40],

    // DMAC.
    istr: u8,
    cntr: u8,
    wtc: u32,
    acr: u32,
    dawr: u16,
    dma_on: bool,
    /// One-time warning latch for DMA to an unmapped bus address.
    dma_warned: bool,
    /// Pending DMA-complete interrupt, modelled as a colour-clock delay
    /// matching the single-speed transfer time.
    dma_done_delay_cck: i64,

    // 6525 TPI.
    tp_a: u8,
    tp_b: u8,
    tp_c: u8,
    tp_ad: u8,
    tp_bd: u8,
    tp_cd: u8,
    tp_imask: u8,
    tp_cr: u8,
    tp_air: u8,
    tp_ilatch: u8,
    tp_ilatch2: u8,

    // Handshake lines. STEN uses the reference's small negative
    // countdown so each response byte produces one clean edge.
    stch: i8,
    sten: i8,
    scor: i8,
    sbcp: i8,
    activate_stch: bool,

    // Drive command channel.
    command_in: [u8; 16],
    command_cnt_in: usize,
    command_out: [u8; 16],
    command_cnt_out: i32,
    command_size_out: usize,
    command_done: bool,

    // Drive/disc state.
    disc: Option<CdImage>,
    cd_media: bool,
    cd_motor: bool,
    cd_playing: bool,
    cd_paused: bool,
    cd_error: bool,
    cd_finished: bool,
    cd_isready: bool,
    sector_size: u32,

    // Data read state.
    read_offset: u64,
    read_length: u64,

    // Audio playback (streams into the host mixer ring).
    play_position: i64,
    play_end: i64,
    last_play_pos: i64,
    audio_status: u8,
    flush_cd_audio: bool,

    // Pacing.
    line_counter_cck: i32,
    lines_until_frame: i32,
    audio_counter_cck: i32,
    booted: bool,

    /// Battery-backed bookmark RAM at $DC8000 (16 KiB, mirrored across
    /// the $DC8000-$DCFFFF window). Session-only; not yet persisted.
    battram: Vec<u8>,

    /// A disc waiting in the tray: inserted (media-change STCH) once
    /// `insert_delay_cck` of emulated time has elapsed. Some discs only
    /// boot when inserted after the CDTV boot screen (verified against
    /// FS-UAE's delayed-insert option and real A570 behaviour).
    pending_disc: Option<CdImage>,
    insert_delay_cck: i64,
}

impl CdtvController {
    pub fn new() -> Self {
        let mut controller = Self {
            configured_base: None,
            shut_up: false,
            config_rom: [0xFF; 0x40],
            istr: 0,
            cntr: 0,
            wtc: 0,
            acr: 0,
            dawr: 0,
            dma_on: false,
            dma_warned: false,
            dma_done_delay_cck: -1,
            tp_a: 0,
            tp_b: 0,
            tp_c: 0,
            tp_ad: 0,
            tp_bd: 0,
            tp_cd: 0,
            tp_imask: 0,
            tp_cr: 0,
            tp_air: 0,
            tp_ilatch: 0,
            tp_ilatch2: 0,
            stch: 0,
            sten: 0,
            scor: 0,
            sbcp: 0,
            activate_stch: false,
            command_in: [0; 16],
            command_cnt_in: 0,
            command_out: [0; 16],
            command_cnt_out: -1,
            command_size_out: 0,
            command_done: false,
            disc: None,
            cd_media: false,
            cd_motor: false,
            cd_playing: false,
            cd_paused: false,
            cd_error: false,
            cd_finished: false,
            cd_isready: false,
            sector_size: 2048,
            read_offset: 0,
            read_length: 0,
            play_position: 0,
            play_end: 0,
            last_play_pos: 0,
            audio_status: AUDIO_STATUS_NO_STATUS,
            flush_cd_audio: false,
            line_counter_cck: 0,
            lines_until_frame: 0,
            audio_counter_cck: 0,
            booted: false,
            battram: vec![0u8; 16 * 1024],
            pending_disc: None,
            insert_delay_cck: -1,
        };
        controller.build_config_rom();
        controller
    }

    pub fn insert_disc(&mut self, disc: CdImage) {
        self.disc = Some(disc);
        self.cd_media = true;
    }

    /// Park a disc in the tray and insert it (with the media-change
    /// status interrupt) after `secs` of emulated time.
    pub fn insert_disc_after(&mut self, disc: CdImage, secs: f64) {
        self.pending_disc = Some(disc);
        self.insert_delay_cck =
            (secs.max(0.0) * f64::from(crate::chipset::paula::PAULA_CLOCK_HZ)) as i64;
    }

    /// Whether a disc is mounted or waiting in the tray.
    pub fn has_disc(&self) -> bool {
        self.disc.is_some() || self.pending_disc.is_some()
    }

    /// Whether the drive is actively working: streaming CD audio, or a
    /// data read in flight (sectors still owed, DMA running, or the
    /// completion interrupt pending). Feeds the status-bar CD LED.
    pub fn activity_led_on(&self) -> bool {
        (self.cd_playing && !self.cd_paused)
            || self.read_length > 0
            || self.dma_on
            || self.dma_done_delay_cck >= 0
    }

    /// Remove the disc (and any disc still waiting in the tray): stop
    /// playback, drop buffered audio, and raise the media-change status
    /// interrupt so cdtv.device notices the removal.
    pub fn eject_disc(&mut self) {
        self.pending_disc = None;
        self.insert_delay_cck = -1;
        let had_media = self.disc.take().is_some() || self.cd_media;
        if !had_media {
            return;
        }
        self.stop_audio();
        self.cd_media = false;
        self.cd_motor = false;
        self.read_length = 0;
        self.audio_status = AUDIO_STATUS_NO_STATUS;
        self.activate_stch = true;
        log::info!("cdtv: disc ejected, media-change STCH raised");
    }

    /// Reset on system reset; keeps the drive state (mounted or still
    /// pending disc) and the battery-backed bookmark RAM contents.
    pub fn reset(&mut self) {
        let disc = self.disc.take();
        let pending = self.pending_disc.take();
        let delay = self.insert_delay_cck;
        let battram = std::mem::take(&mut self.battram);
        *self = Self::new();
        self.battram = battram;
        if let Some(disc) = disc {
            self.insert_disc(disc);
        }
        self.pending_disc = pending;
        self.insert_delay_cck = delay;
    }

    /// Battery-backed RAM window at $DC8000-$DCFFFF.
    pub fn battram_read(&self, addr: u32, size: usize) -> u32 {
        let mut value = 0u32;
        for i in 0..size as u32 {
            let off = ((addr + i) as usize) & (self.battram.len() - 1);
            value = (value << 8) | u32::from(self.battram[off]);
        }
        value
    }

    pub fn battram_write(&mut self, addr: u32, size: usize, value: u32) {
        for i in 0..size as u32 {
            let shift = 8 * (size as u32 - 1 - i);
            let off = ((addr + i) as usize) & (self.battram.len() - 1);
            self.battram[off] = (value >> shift) as u8;
        }
    }

    /// The INT2 (PORTS) line: the 6525's interrupt output plus the
    /// DMAC's enabled end-of-process interrupt, level-fed like Gayle's.
    pub fn int2_line(&self) -> bool {
        if self.tp_ilatch & (1 << 5) != 0 {
            return true;
        }
        // Port C as plain I/O can drive the line directly.
        if self.tp_cr & 1 == 0 && self.tp_c & (1 << 5) != 0 {
            return true;
        }
        self.cntr & CNTR_INTEN != 0 && self.istr & ISTR_E_INT != 0
    }

    // ----- autoconfig ------------------------------------------------------

    /// In the configuration window until configured or shut up.
    pub fn in_config_space(&self) -> bool {
        self.configured_base.is_none() && !self.shut_up
    }

    /// The 64 KiB window this board owns once configured.
    pub fn maps_address(&self, addr: u32) -> bool {
        self.configured_base
            .is_some_and(|base| addr.wrapping_sub(base) < 0x1_0000)
    }

    fn build_config_rom(&mut self) {
        // Per the reference: type $C1 (Zorro II, 64K), product 3,
        // flags $40, manufacturer $0202 (Commodore), serial 0. The
        // special offsets ($00/$40 families) are stored uninverted.
        let mut ew = |addr: usize, value: u8, invert: bool| {
            let hi = value & 0xF0;
            let lo = (value & 0x0F) << 4;
            if invert {
                self.config_rom[addr] = !hi;
                self.config_rom[addr + 2] = !lo;
            } else {
                self.config_rom[addr] = hi;
                self.config_rom[addr + 2] = lo;
            }
        };
        ew(0x00, 0xC1, false);
        ew(0x04, 0x03, true);
        ew(0x08, 0x40, true);
        ew(0x10, 0x02, true);
        ew(0x14, 0x02, true);
        ew(0x18, 0x00, true);
        ew(0x1C, 0x00, true);
        ew(0x20, 0x00, true);
        ew(0x24, 0x00, true);
    }

    pub fn config_read(&self, addr: u32, size: usize) -> u32 {
        let mut value = 0u32;
        for i in 0..size as u32 {
            let off = ((addr + i) & 0xFFFF) as usize;
            let byte = if off < 0x40 {
                self.config_rom[off]
            } else {
                0xFF
            };
            value = (value << 8) | u32::from(byte);
        }
        value
    }

    pub fn config_write(&mut self, addr: u32, size: usize, value: u32) {
        let off = addr & 0xFFFF;
        let byte = (value >> (8 * (size as u32 - 1))) as u8;
        match off {
            0x48 => {
                let base = u32::from(byte) << 16;
                self.configured_base = Some(base);
                log::info!("cdtv: DMAC autoconfigured at {base:#010X}");
            }
            0x4C => {
                self.shut_up = true;
                log::warn!("cdtv: DMAC shut up by ROM autoconfig");
            }
            _ => {}
        }
    }

    // ----- mapped window ---------------------------------------------------

    pub fn read(&mut self, addr: u32, size: usize) -> u32 {
        let mut value = 0u32;
        for i in 0..size as u32 {
            value = (value << 8) | u32::from(self.read_byte((addr + i) & 0xFFFF));
        }
        value
    }

    pub fn write(&mut self, addr: u32, size: usize, value: u32, mem: &mut Memory) {
        for i in 0..size as u32 {
            let shift = 8 * (size as u32 - 1 - i);
            self.write_byte((addr + i) & 0xFFFF, (value >> shift) as u8, mem);
        }
    }

    fn read_byte(&mut self, addr: u32) -> u8 {
        if addr < 0x40 {
            return self.config_rom[addr as usize];
        }
        if crate::envcfg::flag("COPPERLINE_DIAG_CDTV")
            && !(0xB0..0xC0).contains(&addr)
            && addr != 0x41
        {
            log::info!("cdtv rd {addr:02X}");
        }
        if (0xB0..0xC0).contains(&addr) {
            let reg = (addr - 0xB0) / 2;
            let v = self.tp_read(reg);
            if crate::envcfg::flag("COPPERLINE_DIAG_CDTV") {
                log::info!("cdtv tp rd {reg:X}->{v:02X}");
            }
            return v;
        }
        let v = match addr {
            0x41 => {
                let mut v = self.istr;
                if v != 0 {
                    v |= ISTR_INT_P;
                }
                self.istr &= !0x0F;
                if crate::envcfg::flag("COPPERLINE_DIAG_CDTV") {
                    log::info!("cdtv rd 41 -> {v:02X}");
                }
                v
            }
            0x43 => self.cntr,
            0xA1 => {
                self.sten = 0;
                if self.command_cnt_out >= 0 {
                    let v = self.command_out[self.command_cnt_out as usize];
                    self.command_out[self.command_cnt_out as usize] = 0;
                    self.command_cnt_out += 1;
                    if self.command_cnt_out as usize >= self.command_size_out {
                        self.command_size_out = 0;
                        self.command_cnt_out = -1;
                        self.sten = 0;
                    } else {
                        self.sten = 1;
                    }
                    self.tp_check_interrupts();
                    v
                } else {
                    0
                }
            }
            0xE8 | 0xE9 => {
                self.istr |= ISTR_FE_FLG;
                0
            }
            0xA3 | 0xA5 | 0xA7 => 0xFF,
            _ => 0,
        };
        self.tp_check_interrupts();
        v
    }

    fn write_byte(&mut self, addr: u32, value: u8, mem: &mut Memory) {
        if (0xB0..0xC0).contains(&addr) {
            if crate::envcfg::flag("COPPERLINE_DIAG_CDTV") {
                log::info!("cdtv tp wr {:X}={value:02X}", (addr - 0xB0) / 2);
            }
            self.tp_write((addr - 0xB0) / 2, value);
            return;
        }
        if crate::envcfg::flag("COPPERLINE_DIAG_CDTV") {
            log::info!("cdtv wr {addr:02X}={value:02X}");
        }
        match addr {
            0x43 => {
                self.cntr = value;
                if value & CNTR_PREST != 0 {
                    self.reset_drive_state();
                }
            }
            0x80 => self.wtc = (self.wtc & 0x00FF_FFFF) | (u32::from(value) << 24),
            0x81 => self.wtc = (self.wtc & 0xFF00_FFFF) | (u32::from(value) << 16),
            0x82 => self.wtc = (self.wtc & 0xFFFF_00FF) | (u32::from(value) << 8),
            0x83 => self.wtc = (self.wtc & 0xFFFF_FF00) | u32::from(value),
            0x84 => self.acr = (self.acr & 0x00FF_FFFF) | (u32::from(value) << 24),
            0x85 => self.acr = (self.acr & 0xFF00_FFFF) | (u32::from(value) << 16),
            0x86 => self.acr = (self.acr & 0xFFFF_00FF) | (u32::from(value) << 8),
            0x87 => self.acr = (self.acr & 0xFFFF_FF01) | u32::from(value & !1),
            0x8E => self.dawr = (self.dawr & 0x00FF) | (u16::from(value) << 8),
            0x8F => self.dawr = (self.dawr & 0xFF00) | u16::from(value),
            0xA1 => self.drive_command_byte(value),
            0xE0 | 0xE1 => {
                // A word write strobes both byte lanes; only start once
                // (and a consumed WTC of zero is not a new transfer).
                if !self.dma_on && self.wtc != 0 {
                    self.dma_on = true;
                    if self.cntr & CNTR_PDMD == 0 {
                        self.run_dma(mem);
                    }
                }
            }
            0xE2 | 0xE3 => {
                self.dma_on = false;
                self.dma_done_delay_cck = -1;
            }
            0xE4 | 0xE5 => {
                self.istr = 0;
            }
            0xE8 | 0xE9 => self.istr |= ISTR_FE_FLG,
            _ => {}
        }
        self.tp_check_interrupts();
    }

    fn reset_drive_state(&mut self) {
        log::debug!("cdtv: drive reset");
        self.stop_audio();
        self.cd_playing = false;
        self.cd_paused = false;
        self.cd_motor = false;
        self.cd_error = false;
        self.cd_finished = false;
        self.stch = 1;
    }

    // ----- 6525 TPI --------------------------------------------------------

    fn get_tp_c(&self) -> u8 {
        (if self.sbcp != 0 { 0 } else { 1 << 0 })
            | (if self.scor != 0 { 0 } else { 1 << 1 })
            | (if self.stch != 0 { 0 } else { 1 << 2 })
            | (if self.sten != 0 {
                0
            } else {
                (1 << 3) | (1 << 4)
            })
    }

    /// Edge-consuming variant for the interrupt latch path.
    fn get_tp_c_level(&mut self) -> u8 {
        let v = (if self.sbcp == 1 { 0 } else { 1 << 0 })
            | (if self.scor == 1 { 0 } else { 1 << 1 })
            | (if self.stch == 1 { 0 } else { 1 << 2 })
            | (if self.sten == 1 { 0 } else { 1 << 3 })
            | (1 << 4);
        if self.sten == 1 {
            self.sten = -1;
        }
        if self.scor == 1 {
            self.scor = -1;
        }
        if self.sbcp == 1 {
            self.sbcp = -1;
        }
        v
    }

    fn tp_check_interrupts(&mut self) {
        // MC = 0: plain I/O mode, no interrupt latching.
        if self.tp_cr & 1 != 1 {
            self.get_tp_c_level();
            return;
        }
        self.tp_ilatch |= self.get_tp_c_level() ^ 0x1F;
        self.stch = 0;
        if self.tp_ilatch & (1 << 5) == 0 && self.tp_ilatch & self.tp_imask != 0 {
            self.tp_air = 0;
            let mut mask = 0x10u8;
            while (self.tp_ilatch & self.tp_imask) & mask == 0 {
                mask >>= 1;
            }
            self.tp_air |= self.tp_ilatch & mask;
            self.tp_ilatch |= 1 << 5; // interrupt out
            self.tp_ilatch2 = self.tp_ilatch & mask;
            self.tp_ilatch &= !mask;
        }
    }

    fn tp_read(&mut self, reg: u32) -> u8 {
        let v = match reg {
            // Port A: subcode byte input; no payload modelled.
            0 => 0,
            1 => self.tp_b,
            2 => {
                if self.tp_cr & 1 != 0 {
                    self.tp_ilatch | self.tp_ilatch2
                } else {
                    self.get_tp_c()
                }
            }
            3 => self.tp_ad,
            4 => self.tp_bd,
            5 => {
                if self.tp_cr & 1 != 0 {
                    self.tp_imask
                } else {
                    self.tp_cd
                }
            }
            6 => self.tp_cr,
            7 => {
                let v = self.tp_air;
                if self.tp_cr & 1 != 0 {
                    self.tp_ilatch &= !(1 << 5);
                    self.tp_ilatch2 = 0;
                }
                self.tp_air = 0;
                v
            }
            _ => 0,
        };
        self.tp_check_interrupts();
        v
    }

    fn tp_write(&mut self, reg: u32, value: u8) {
        match reg {
            0 => self.tp_a = value,
            1 => self.tp_b = value,
            2 => {
                if self.tp_cr & 1 != 0 {
                    // Interrupt latch: writing 0 bits clears them.
                    self.tp_ilatch &= 0xE0 | value;
                } else {
                    self.tp_c = (self.get_tp_c() & !self.tp_cd) | (value & self.tp_cd);
                }
            }
            3 => self.tp_ad = value,
            4 => self.tp_bd = value,
            5 => {
                if self.tp_cr & 1 != 0 {
                    self.tp_imask = value & 0x1F;
                } else {
                    self.tp_cd = value;
                }
            }
            6 => self.tp_cr = value,
            7 => self.tp_air = value,
            _ => {}
        }
        // CMD/ENABLE/XAEN/DTEN come from port B; only the DAC volume
        // strobes (bits 5-7) would matter beyond this and are ignored
        // (host CD volume is fixed).
        self.tp_check_interrupts();
    }

    // ----- drive commands --------------------------------------------------

    fn drive_command_byte(&mut self, byte: u8) {
        if self.command_cnt_in < self.command_in.len() {
            self.command_in[self.command_cnt_in] = byte;
        }
        self.command_cnt_in += 1;
        let cmd = self.command_in[0];
        let needed: usize = match cmd {
            0x00 | 0x80 => 2,
            0x81 | 0x85 | 0x86 | 0x88 | 0xA2 => 1,
            _ => 7,
        };
        if self.command_cnt_in < needed {
            return;
        }
        self.execute_drive_command();
        self.command_cnt_in = 0;
    }

    fn accept(&mut self, size: i32) {
        self.command_cnt_out = 0;
        self.command_size_out = size.max(0) as usize;
        self.command_done = true;
        if size < 0 {
            self.cd_error = true;
        }
    }

    fn execute_drive_command(&mut self) {
        let cmd = self.command_in[0];
        let input = self.command_in;
        log::debug!(
            "cdtv: drive cmd {:02X?}",
            &input[..self.command_cnt_in.min(7)]
        );
        self.command_out = [0; 16];
        match cmd {
            0x00 | 0x80 => {
                self.command_out[0] = 0xAA;
                self.command_out[1] = 0x55;
                self.accept(2);
            }
            0x01 => {
                // Seek.
                self.cd_finished = true;
                self.activate_stch = true;
                self.accept(0);
            }
            0x02 => {
                // Read data sectors.
                let start =
                    (u32::from(input[1]) << 16) | (u32::from(input[2]) << 8) | u32::from(input[3]);
                let length = (u32::from(input[4]) << 8) | u32::from(input[5]);
                if self.cd_playing {
                    self.stop_audio();
                }
                log::debug!("cdtv: READ DATA {start} +{length} sectors");
                self.read_offset = u64::from(start) * u64::from(self.sector_size);
                self.read_length = u64::from(length) * u64::from(self.sector_size);
                self.cd_motor = true;
                self.audio_status = AUDIO_STATUS_NOT_SUPPORTED;
                self.accept(0);
            }
            0x04 => {
                self.cd_motor = true;
                self.cd_finished = true;
                self.accept(0);
            }
            0x05 => {
                self.cd_motor = false;
                self.cd_finished = true;
                self.accept(0);
            }
            0x09 | 0x0A => {
                // Play audio by LSN (09) or MSF (0A).
                let mut start =
                    (u32::from(input[1]) << 16) | (u32::from(input[2]) << 8) | u32::from(input[3]);
                let mut end =
                    (u32::from(input[4]) << 16) | (u32::from(input[5]) << 8) | u32::from(input[6]);
                if cmd == 0x09 {
                    end += start;
                } else {
                    start = msf_to_lsn(start);
                    if end < 0x00FF_FFFF {
                        end = msf_to_lsn(end);
                    }
                }
                self.play_range(start, end);
                self.accept(0);
            }
            0x0B => {
                // Play by track range.
                let track_start = input[1];
                let track_end = input[3];
                if track_start == 0 && track_end == 0 {
                    self.accept(0);
                } else {
                    let (start, end) = self.track_range(track_start, track_end);
                    match start {
                        Some(start) => {
                            self.play_range(start, end);
                        }
                        None => {
                            self.stop_audio();
                            self.cd_error = true;
                            self.activate_stch = true;
                        }
                    }
                    self.accept(0);
                }
            }
            0x81 => {
                let mut flag = 0u8;
                if !self.cd_isready {
                    flag |= 1 << 0;
                }
                if self.cd_playing {
                    flag |= 1 << 2;
                }
                if self.cd_finished {
                    flag |= 1 << 3;
                }
                if self.cd_error {
                    flag |= 1 << 4;
                }
                if self.cd_motor {
                    flag |= 1 << 5;
                }
                if self.cd_media {
                    flag |= 1 << 6;
                }
                self.command_out[0] = flag;
                self.accept(1);
                self.cd_finished = false;
            }
            0x82 => {
                if self.cd_error {
                    self.command_out[2] |= 1 << 4;
                }
                self.cd_error = false;
                self.cd_isready = false;
                self.cd_finished = true;
                self.accept(6);
            }
            0x83 => {
                self.command_out[..MODEL_NAME.len()].copy_from_slice(MODEL_NAME);
                self.cd_finished = true;
                self.accept(MODEL_NAME.len() as i32);
            }
            0x84 => {
                let size = (u32::from(input[2]) << 8) | u32::from(input[3]);
                match size {
                    512 | 1024 | 2048 | 2052 | 2336 | 2340 | 2352 => self.sector_size = size,
                    _ => {
                        log::warn!("cdtv: unsupported sector size {size}");
                        self.cd_error = true;
                    }
                }
                self.cd_finished = true;
                self.accept(0);
            }
            0x85 => {
                self.command_out[0] = (self.sector_size >> 8) as u8;
                self.command_out[1] = self.sector_size as u8;
                self.accept(2);
            }
            0x86 => {
                let last = self.disc.as_ref().map(|d| d.total_sectors()).unwrap_or(0);
                let size = last.saturating_sub(1);
                self.command_out[0] = (size >> 16) as u8;
                self.command_out[1] = (size >> 8) as u8;
                self.command_out[2] = size as u8;
                self.command_out[3] = (self.sector_size >> 8) as u8;
                self.command_out[4] = self.sector_size as u8;
                if self.cd_media {
                    self.accept(5);
                } else {
                    self.cd_error = true;
                    self.accept(-1);
                }
            }
            0x87 => {
                let len = self.subq_response(input[1] & 2 != 0);
                self.accept(len);
            }
            0x88 => {
                self.accept(14);
            }
            0x89 => {
                let len = self.info_response();
                self.accept(len);
            }
            0x8A => {
                let len = self.toc_response(input[2], input[1] & 2 != 0);
                self.accept(len);
            }
            0x8B => {
                let pause = input[1] == 0x00;
                if self.cd_playing {
                    self.cd_paused = pause;
                    self.audio_status = if pause {
                        AUDIO_STATUS_PAUSED
                    } else {
                        AUDIO_STATUS_IN_PROGRESS
                    };
                } else {
                    self.cd_paused = false;
                    self.audio_status = AUDIO_STATUS_NO_STATUS;
                }
                self.cd_finished = true;
                self.accept(0);
            }
            0xA2 => {
                self.accept(4);
            }
            0xA3 => {
                self.cd_finished = true;
                self.accept(0);
            }
            other => {
                log::warn!("cdtv: unknown drive command {other:02X}");
                self.cd_error = true;
                self.accept(0);
            }
        }
    }

    fn play_range(&mut self, start: u32, mut end: u32) {
        if start == 0 && end == 0 {
            self.cd_finished = self.cd_playing;
            self.stop_audio();
            self.cd_motor = false;
            self.audio_status = AUDIO_STATUS_NO_STATUS;
            self.cd_error = true;
            self.activate_stch = true;
            return;
        }
        let last = self.disc.as_ref().map(|d| d.total_sectors()).unwrap_or(0);
        if end >= 0x00FF_FFFF || end > last {
            end = last;
        }
        log::debug!("cdtv: PLAY {start}..{end}");
        self.play_position = i64::from(start);
        self.play_end = i64::from(end);
        self.last_play_pos = i64::from(start);
        self.cd_playing = true;
        self.cd_paused = false;
        self.cd_motor = true;
        self.audio_status = AUDIO_STATUS_IN_PROGRESS;
        self.activate_stch = true;
        self.flush_cd_audio = true;
    }

    fn track_range(&self, track_start: u8, track_end: u8) -> (Option<u32>, u32) {
        let Some(disc) = self.disc.as_ref() else {
            return (None, 0);
        };
        let mut start = None;
        let mut end = disc.total_sectors();
        for track in disc.tracks() {
            if track.number == track_start {
                start = Some(track.start_sector);
            }
            if track.number == track_end {
                end = track.start_sector;
            }
        }
        (start, end)
    }

    /// Explicit stop: abandon playback and drop any buffered samples.
    fn stop_audio(&mut self) {
        self.finish_audio();
        self.flush_cd_audio = true;
    }

    /// Playback ended (naturally or by command); buffered samples are
    /// left to drain through the mixer.
    fn finish_audio(&mut self) {
        if self.cd_playing {
            self.audio_status = AUDIO_STATUS_PLAY_COMPLETE;
            self.cd_finished = true;
        }
        self.cd_playing = false;
        self.cd_paused = false;
    }

    fn subq_response(&mut self, msf: bool) -> i32 {
        let pos = self.last_play_pos.max(0) as u32;
        let (track, track_start) = self.track_for(pos);
        self.command_out[0] = self.audio_status;
        self.command_out[1] = 0x01; // CtlAdr (audio, ADR 1), pre-swapped
        self.command_out[2] = track;
        self.command_out[3] = 1; // index
        let diskpos = if msf { lsn_to_msf(pos) } else { pos };
        let trackrel = pos.saturating_sub(track_start);
        let trackpos = if msf { lsn_to_msf(trackrel) } else { trackrel };
        self.command_out[4] = 0;
        self.command_out[5] = (diskpos >> 16) as u8;
        self.command_out[6] = (diskpos >> 8) as u8;
        self.command_out[7] = diskpos as u8;
        self.command_out[8] = 0;
        self.command_out[9] = (trackpos >> 16) as u8;
        self.command_out[10] = (trackpos >> 8) as u8;
        self.command_out[11] = trackpos as u8;
        self.command_out[12] = 0;
        13
    }

    fn track_for(&self, pos: u32) -> (u8, u32) {
        let Some(disc) = self.disc.as_ref() else {
            return (1, 0);
        };
        let mut current = (1u8, 0u32);
        for track in disc.tracks() {
            if pos >= track.start_sector {
                current = (track.number, track.start_sector);
            }
        }
        current
    }

    fn info_response(&mut self) -> i32 {
        let Some(disc) = self.disc.as_ref() else {
            return -1;
        };
        let first = disc.tracks().first().map(|t| t.number).unwrap_or(1);
        let last = disc.tracks().last().map(|t| t.number).unwrap_or(1);
        let size = lsn_to_msf(disc.total_sectors());
        self.cd_motor = true;
        self.command_out[0] = first;
        self.command_out[1] = last;
        self.command_out[2] = (size >> 16) as u8;
        self.command_out[3] = (size >> 8) as u8;
        self.command_out[4] = size as u8;
        self.cd_finished = true;
        5
    }

    fn toc_response(&mut self, point: u8, msf: bool) -> i32 {
        let Some(disc) = self.disc.as_ref() else {
            return -1;
        };
        let track_count = disc.tracks().len() as u8;
        let total = disc.total_sectors();
        let first = disc.tracks().first().map(|t| t.number).unwrap_or(1);
        let last = disc.tracks().last().map(|t| t.number).unwrap_or(1);
        // Session points plus per-track entries, as read_toc serves them.
        let entry: Option<(u8, u8, u32)> = match point {
            0xA0 => Some((0x01, 0xA0, u32::from(first) << 16)),
            0xA1 => Some((0x01, 0xA1, u32::from(last) << 16)),
            0xA2 => Some((0x01, 0xA2, if msf { lsn_to_msf(total) } else { total })),
            p => disc.tracks().iter().find(|t| t.number == p).map(|t| {
                let addr = if msf {
                    lsn_to_msf(t.start_sector)
                } else {
                    t.start_sector
                };
                let control = if t.kind.is_data() { 0x04 } else { 0x00 };
                ((0x01 << 4) | control, to_bcd(t.number), addr)
            }),
        };
        let Some((ctladr, point_out, addr)) = entry else {
            return -1;
        };
        self.cd_motor = true;
        self.command_out[0] = 0;
        self.command_out[1] = ctladr;
        self.command_out[2] = point_out;
        self.command_out[3] = track_count;
        self.command_out[4] = 0;
        self.command_out[5] = (addr >> 16) as u8;
        self.command_out[6] = (addr >> 8) as u8;
        self.command_out[7] = addr as u8;
        self.cd_finished = true;
        8
    }

    // ----- DMA -------------------------------------------------------------

    /// The reference performs the whole transfer when DMA starts, then
    /// delays the completion interrupt by the single-speed read time.
    /// One word of sector data onto the system bus. The DMAC is a 24-bit
    /// bus master like the A2091's: it reaches chip RAM, slow RAM, and
    /// Zorro II board RAM (the CD driver allocates CD buffers in fast RAM when
    /// present). Unmapped targets warn once and drop the data.
    fn dma_write_word(&mut self, mem: &mut Memory, addr: u32, hi: u8, lo: u8) {
        let w = (u16::from(hi) << 8) | u16::from(lo);
        if !crate::zorro_device::dma_write_word(mem, addr, w) && !self.dma_warned {
            self.dma_warned = true;
            log::warn!("cdtv: DMA to unmapped address {addr:#08X}; ignoring");
        }
    }

    fn dma_read_byte(mem: &Memory, addr: u32) -> u8 {
        crate::zorro_device::dma_read_byte(mem, addr).unwrap_or(0xFF)
    }

    fn run_dma(&mut self, mem: &mut Memory) {
        let words = self.wtc;
        if self.disc.is_none() {
            self.dma_on = false;
            return;
        }
        let sector_size = u64::from(self.sector_size);
        if sector_size < 2048 {
            log::warn!("cdtv: DMA with sector size {sector_size} not supported");
            self.dma_on = false;
            return;
        }
        let mut raw = [0u8; RAW_SECTOR_BYTES];
        let mut current_sector = u32::MAX;
        let mut remaining = words;
        while remaining > 0 && self.dma_on {
            let sector = (self.read_offset / sector_size) as u32;
            let in_sector = (self.read_offset % sector_size) as usize;
            if sector != current_sector {
                let disc = self.disc.as_mut().expect("checked above");
                let ok = if sector_size == 2048 {
                    let mut data = [0u8; 2048];
                    let r = disc.read_data_sector(sector, &mut data);
                    raw[..2048].copy_from_slice(&data);
                    r
                } else {
                    disc.read_raw_sector(sector, &mut raw)
                };
                if ok.is_err() {
                    log::warn!("cdtv: CD read error at sector {sector}");
                    self.cd_error = true;
                    self.activate_stch = true;
                    break;
                }
                current_sector = sector;
            }
            self.dma_write_word(mem, self.acr, raw[in_sector], raw[in_sector + 1]);
            self.acr = self.acr.wrapping_add(2);
            self.read_offset += 2;
            self.read_length = self.read_length.saturating_sub(2);
            remaining -= 1;
        }
        if crate::envcfg::flag("COPPERLINE_DIAG_CDTV") {
            let base = self.acr.wrapping_sub(words * 2);
            let peek: Vec<u8> = (0..8)
                .map(|i| Self::dma_read_byte(mem, base.wrapping_add(i)))
                .collect();
            log::info!("cdtv DMA done: {words} words to {base:#08X}, first bytes {peek:02X?}");
        }
        self.wtc = 0;
        self.dma_on = false;
        self.cd_finished = true;
        // Completion interrupt after the 1x read time for the words moved.
        let bytes = u64::from(words) * 2;
        self.dma_done_delay_cck =
            (bytes * u64::from(CCK_PER_CD_FRAME) / 2048).max(CCK_PER_LINE as u64) as i64;
    }

    // ----- periodic work ---------------------------------------------------

    pub fn tick(&mut self, cck: u32, cd_audio: &mut CdAudioRing) {
        if self.configured_base.is_none() {
            return;
        }

        // Delayed disc insert: media change once the timer runs out.
        if self.pending_disc.is_some() {
            self.insert_delay_cck -= i64::from(cck);
            if self.insert_delay_cck < 0 {
                self.disc = self.pending_disc.take();
                self.cd_media = true;
                self.activate_stch = true;
                log::info!("cdtv: disc inserted (delayed), media-change STCH raised");
            }
        }

        if self.flush_cd_audio {
            self.flush_cd_audio = false;
            cd_audio.clear();
        }

        // DMA completion interrupt after the modelled read time.
        if self.dma_done_delay_cck >= 0 {
            self.dma_done_delay_cck -= i64::from(cck);
            if self.dma_done_delay_cck < 0
                && self.cntr & (CNTR_INTEN | CNTR_TCEN) == (CNTR_INTEN | CNTR_TCEN)
            {
                self.istr |= ISTR_INT_P | ISTR_E_INT;
                if crate::envcfg::flag("COPPERLINE_DIAG_CDTV") {
                    log::info!("cdtv E_INT fired");
                }
            }
        }

        // CD audio: stream one sector per CD frame at single speed.
        self.audio_counter_cck -= cck as i32;
        if self.audio_counter_cck <= 0 {
            self.audio_counter_cck += CCK_PER_CD_FRAME as i32;
            self.stream_audio_sector(cd_audio);
        }

        // Per-scanline housekeeping, as the reference's hsync handler.
        self.line_counter_cck -= cck as i32;
        while self.line_counter_cck <= 0 {
            self.line_counter_cck += CCK_PER_LINE as i32;
            self.line_tick();
        }
    }

    fn line_tick(&mut self) {
        if !self.booted {
            // Initial status change: media present notification path.
            self.booted = true;
            self.activate_stch = true;
        }

        if self.command_done {
            self.command_done = false;
            self.sten = 1;
            self.stch = 0;
            self.tp_check_interrupts();
        }
        if self.sten < 0 {
            self.sten -= 1;
            if self.sten < -3 {
                self.sten = 0;
            }
        }

        self.lines_until_frame -= 1;
        if self.lines_until_frame <= 0 {
            // 75 Hz frame pulse: SCOR fires continuously while not
            // playing (the reference notes these happen all the time).
            self.lines_until_frame = (CCK_PER_CD_FRAME / CCK_PER_LINE) as i32;
            if self.scor == 0 && !self.cd_playing {
                self.scor = 1;
                self.tp_check_interrupts();
                self.scor = 0;
            }
        }

        if self.activate_stch {
            self.do_stch();
        }
    }

    fn do_stch(&mut self) {
        if self.tp_cr & 1 != 0 && self.tp_air & (1 << 2) == 0 {
            self.stch = 1;
            self.activate_stch = false;
            self.tp_check_interrupts();
        }
    }

    fn stream_audio_sector(&mut self, cd_audio: &mut CdAudioRing) {
        if !self.cd_playing || self.cd_paused {
            return;
        }
        if !cd_audio.wants_sector() {
            return;
        }
        let Some(disc) = self.disc.as_mut() else {
            return;
        };
        if self.play_position >= self.play_end || self.play_position < 0 {
            self.finish_audio();
            self.activate_stch = true;
            return;
        }
        let sector = self.play_position as u32;
        if disc.is_audio_sector(sector) {
            let mut raw = [0u8; RAW_SECTOR_BYTES];
            if disc.read_audio_sector(sector, &mut raw).is_ok() {
                cd_audio.push_sector(&raw);
            }
        }
        self.play_position += 1;
        self.last_play_pos = self.play_position;
    }
}

impl crate::zorro_device::ZorroDevice for CdtvController {
    // The CDTV self-decodes its window (base is 64K-aligned), so the dispatch
    // passes the window offset, which `read_byte`/`write_byte` mask to the same
    // low 16 bits the absolute address carried before.
    fn read(&mut self, off: u32, size: usize, _host: &mut crate::zorro_device::DeviceHost) -> u32 {
        Self::read(self, off, size)
    }

    fn write(
        &mut self,
        off: u32,
        size: usize,
        value: u32,
        host: &mut crate::zorro_device::DeviceHost,
    ) {
        Self::write(self, off, size, value, host.memory_mut())
    }

    // The DMAC tick streams CD audio rather than touching guest memory, so it
    // draws Paula's CD-audio ring from the host instead of `Memory`.
    fn tick(&mut self, cck: u32, host: &mut crate::zorro_device::DeviceHost) {
        Self::tick(self, cck, host.cd_audio())
    }

    fn int2_line(&self) -> bool {
        Self::int2_line(self)
    }

    // Ticked every slice to advance CD audio and deliver the delayed
    // DMA-completion interrupt (matches its pre-trait unconditional tick).
    fn is_idle(&self) -> bool {
        false
    }

    fn reset(&mut self) {
        Self::reset(self)
    }

    fn kind(&self) -> &'static str {
        "cdtv"
    }
}

impl Default for CdtvController {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cdrom::DATA_SECTOR_BYTES;
    use std::path::PathBuf;

    fn test_disc() -> CdImage {
        static UNIQUE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let unique = UNIQUE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir();
        let cue: PathBuf = dir.join(format!(
            "copperline-cdtv-{}-{unique}.cue",
            std::process::id()
        ));
        let bin: PathBuf = dir.join(format!(
            "copperline-cdtv-{}-{unique}.bin",
            std::process::id()
        ));
        let mut bytes = Vec::new();
        for s in 0u8..4 {
            bytes.extend(std::iter::repeat_n(s, DATA_SECTOR_BYTES));
        }
        for _ in 0..2 * (RAW_SECTOR_BYTES / 4) {
            bytes.extend_from_slice(&[0x00, 0x10, 0x00, 0x20]);
        }
        std::fs::write(&bin, &bytes).unwrap();
        std::fs::write(
            &cue,
            format!(
                concat!(
                    "FILE \"{}\" BINARY\n",
                    "  TRACK 01 MODE1/2048\n    INDEX 01 00:00:00\n",
                    "  TRACK 02 AUDIO\n    INDEX 01 00:00:04\n",
                ),
                bin.file_name().unwrap().to_string_lossy()
            ),
        )
        .unwrap();
        let image = CdImage::load(&cue).unwrap();
        let _ = std::fs::remove_file(&cue);
        let _ = std::fs::remove_file(&bin);
        image
    }

    fn configured_controller() -> CdtvController {
        let mut cdtv = CdtvController::new();
        cdtv.insert_disc(test_disc());
        cdtv.config_write(0x48, 1, 0xE9);
        assert_eq!(cdtv.configured_base, Some(0x00E9_0000));
        cdtv
    }

    fn test_mem(chip_bytes: usize) -> Memory {
        Memory {
            chip_ram: vec![0u8; chip_bytes],
            slow_ram: Vec::new(),
            rom: Vec::new(),
            overlay: false,
            zorro: crate::zorro::ZorroChain::default(),
            extended_rom: Vec::new(),
            extended_rom_base: 0,
            wcs: Vec::new(),
            wcs_write_protected: false,
        }
    }

    /// Send a drive command and read back the response bytes via $A1.
    fn command(cdtv: &mut CdtvController, mem: &mut Memory, bytes: &[u8]) -> Vec<u8> {
        for byte in bytes {
            cdtv.write(0xA1, 1, u32::from(*byte), mem);
        }
        let size = cdtv.command_size_out;
        let mut out = Vec::new();
        for _ in 0..size {
            out.push(cdtv.read(0xA1, 1) as u8);
        }
        out
    }

    #[test]
    fn eject_disc_clears_media_and_raises_media_change_stch() {
        let mut cdtv = configured_controller();
        assert!(cdtv.has_disc());
        assert!(cdtv.cd_media);

        cdtv.eject_disc();
        assert!(!cdtv.has_disc());
        assert!(!cdtv.cd_media);
        assert!(cdtv.activate_stch, "eject must raise the media-change STCH");

        // Ejecting an empty drive is a no-op.
        cdtv.activate_stch = false;
        cdtv.eject_disc();
        assert!(!cdtv.activate_stch);

        // Eject also cancels a disc still waiting in the tray.
        cdtv.insert_disc_after(test_disc(), 5.0);
        assert!(cdtv.has_disc());
        cdtv.eject_disc();
        assert!(!cdtv.has_disc());
    }

    #[test]
    fn activity_led_tracks_audio_and_data_work() {
        let mut cdtv = configured_controller();
        assert!(!cdtv.activity_led_on());

        cdtv.cd_playing = true;
        assert!(cdtv.activity_led_on());
        cdtv.cd_paused = true;
        assert!(!cdtv.activity_led_on());
        cdtv.cd_playing = false;
        cdtv.cd_paused = false;

        cdtv.read_length = 2048;
        assert!(cdtv.activity_led_on());
        cdtv.read_length = 0;
        cdtv.dma_on = true;
        assert!(cdtv.activity_led_on());
        cdtv.dma_on = false;
        assert!(!cdtv.activity_led_on());
    }

    #[test]
    fn autoconfig_rom_identifies_commodore_dmac() {
        let cdtv = CdtvController::new();
        // er_Type $C1: Zorro II, 64K board.
        assert_eq!(cdtv.config_read(0x00, 1), 0xC0);
        assert_eq!(cdtv.config_read(0x02, 1), 0x10);
        // Product 3 (inverted nibbles).
        assert_eq!(cdtv.config_read(0x04, 1) & 0xF0, 0xF0);
        assert_eq!(cdtv.config_read(0x06, 1) & 0xF0, 0xC0);
    }

    #[test]
    fn handshake_command_returns_aa55_and_pulses_sten() {
        let mut mem = test_mem(4096);
        let mut cdtv = configured_controller();
        // Put the 6525 into interrupt mode with STEN unmasked.
        cdtv.write(0xBC, 1, 0x01, &mut mem); // CR: MC=1
        cdtv.write(0xBA, 1, 0x08, &mut mem); // imask: STEN

        let out = command(&mut cdtv, &mut mem, &[0x00, 0x00]);
        assert_eq!(out, vec![0xAA, 0x55]);
    }

    #[test]
    fn status_command_reports_media_and_motor() {
        let mut mem = test_mem(4096);
        let mut cdtv = configured_controller();
        let out = command(&mut cdtv, &mut mem, &[0x81]);
        assert_eq!(out.len(), 1);
        assert_ne!(out[0] & 0x40, 0, "media bit should be set");

        // Motor on, then status again.
        let _ = command(&mut cdtv, &mut mem, &[0x04, 0, 0, 0, 0, 0, 0]);
        let out = command(&mut cdtv, &mut mem, &[0x81]);
        assert_ne!(out[0] & 0x20, 0, "motor bit should be set");
    }

    #[test]
    fn read_and_dma_transfers_sector_data_into_chip_ram() {
        let mut mem = test_mem(64 * 1024);
        let mut cdtv = configured_controller();

        // READ sector 1, 1 sector; then DMA 1024 words to $2000.
        let _ = command(&mut cdtv, &mut mem, &[0x02, 0, 0, 1, 0, 1, 0]);
        cdtv.write(0x80, 4, 1024, &mut mem); // WTC
        cdtv.write(0x84, 4, 0x2000, &mut mem); // ACR
        cdtv.write(0x43, 1, u32::from(CNTR_INTEN | CNTR_TCEN), &mut mem);
        cdtv.write(0xE0, 1, 0, &mut mem); // start DMA

        assert!(mem.chip_ram[0x2000..0x2000 + 2048].iter().all(|&b| b == 1));
        // Completion interrupt fires after the modelled read time.
        let mut ring = CdAudioRing::default();
        for _ in 0..80 {
            cdtv.tick(2048, &mut ring);
        }
        assert_ne!(cdtv.istr & ISTR_E_INT, 0);
        assert!(cdtv.int2_line());
    }

    #[test]
    fn dma_reaches_zorro_fast_ram_without_touching_chip_ram() {
        // The DMAC is a 24-bit bus master: when the CD driver allocates the CD
        // buffer in autoconfigured fast RAM, sector data must land on the
        // board, not wrap into chip RAM (which corrupted the display and
        // made the CDTV ROM fall back to the audio player).
        let mut mem = test_mem(64 * 1024);
        mem.zorro
            .add_board_configured_at(crate::zorro::BoardSpec::fast_ram(512 * 1024), 0x0020_0000)
            .unwrap();
        let mut cdtv = configured_controller();

        // READ sector 1, 1 sector; then DMA 1024 words to $200800.
        let _ = command(&mut cdtv, &mut mem, &[0x02, 0, 0, 1, 0, 1, 0]);
        cdtv.write(0x80, 4, 1024, &mut mem); // WTC
        cdtv.write(0x84, 4, 0x0020_0800, &mut mem); // ACR
        cdtv.write(0x43, 1, u32::from(CNTR_INTEN | CNTR_TCEN), &mut mem);
        cdtv.write(0xE0, 1, 0, &mut mem); // start DMA

        let (board, off) = mem.zorro.region_at(0x0020_0800, 2).unwrap();
        let ram = mem.zorro.board_ram(board);
        assert!(ram[off..off + 2048].iter().all(|&b| b == 1));
        assert!(mem.chip_ram.iter().all(|&b| b == 0), "chip RAM untouched");
    }

    #[test]
    fn toc_and_info_commands_describe_the_disc() {
        let mut mem = test_mem(4096);
        let mut cdtv = configured_controller();

        let info = command(&mut cdtv, &mut mem, &[0x89, 0, 0, 0, 0, 0, 0]);
        assert_eq!(info[0], 1, "first track");
        assert_eq!(info[1], 2, "last track");

        // Lead-out point A2 in LSN mode: 6 sectors total.
        let toc = command(&mut cdtv, &mut mem, &[0x8A, 0, 0xA2, 0, 0, 0, 0]);
        let addr = (u32::from(toc[5]) << 16) | (u32::from(toc[6]) << 8) | u32::from(toc[7]);
        assert_eq!(addr, 6);

        // Track 2 is audio at sector 4.
        let toc = command(&mut cdtv, &mut mem, &[0x8A, 0, 0x02, 0, 0, 0, 0]);
        assert_eq!(toc[1] & 0x04, 0, "audio control nibble");
        let addr = (u32::from(toc[5]) << 16) | (u32::from(toc[6]) << 8) | u32::from(toc[7]);
        assert_eq!(addr, 4);
    }

    #[test]
    fn delayed_insert_raises_media_change_after_timer() {
        let mut mem = test_mem(4096);
        let mut ring = CdAudioRing::default();
        let mut cdtv = CdtvController::new();
        cdtv.insert_disc_after(test_disc(), 0.05);
        cdtv.config_write(0x48, 1, 0xE9);
        // Interrupt mode with STCH unmasked, so the insert is observable.
        cdtv.write(0xBC, 1, 0x01, &mut mem);
        cdtv.write(0xBA, 1, 0x04, &mut mem);

        // Before the timer: no media.
        let out = command(&mut cdtv, &mut mem, &[0x81]);
        assert_eq!(out[0] & 0x40, 0, "no media before the delayed insert");

        // ~0.06s of emulated time.
        for _ in 0..110 {
            cdtv.tick(2048, &mut ring);
        }
        assert!(cdtv.cd_media, "media should be inserted");
        assert_ne!(cdtv.tp_ilatch & (1 << 5), 0, "STCH interrupt raised");
        let out = command(&mut cdtv, &mut mem, &[0x81]);
        assert_ne!(out[0] & 0x40, 0, "media bit set after insert");
    }

    #[test]
    fn play_streams_audio_and_completes() {
        let mut mem = test_mem(4096);
        let mut cdtv = configured_controller();
        let mut ring = CdAudioRing::default();

        // PLAY LSN 4, length 2.
        let _ = command(&mut cdtv, &mut mem, &[0x09, 0, 0, 4, 0, 0, 2]);
        assert!(cdtv.cd_playing);
        for _ in 0..16 {
            cdtv.tick(CCK_PER_CD_FRAME / 2, &mut ring);
        }
        assert!(!cdtv.cd_playing, "play should have completed");
        assert_eq!(cdtv.audio_status, AUDIO_STATUS_PLAY_COMPLETE);
        let (left, right) = ring.next_sample();
        assert!((left - 0x1000 as f32 / 32768.0).abs() < 1e-4);
        assert!((right - 0x2000 as f32 / 32768.0).abs() < 1e-4);
    }
}
