// SPDX-License-Identifier: GPL-3.0-or-later

//! Gayle gate array (A600/A1200): the ID register at $DE1000, the IDE
//! interface at $DA0000, the Gayle status/interrupt/config registers at
//! $DA8000-$DAA000, and empty-slot PCMCIA status.
//!
//! Decode and register layout follow the Commodore schematics as captured by
//! the Linux `gayle.c` IDE driver and the ROM scsi.device: the IDE task
//! file lives at $DA0000 with a 4-byte stride (byte registers on the odd
//! word half, offset base+4*reg+2), and the control block register at
//! base+$101A. None of this is on the chip bus; the CPU reaches it through
//! `cpu_external_access`.

use crate::harddrive::{HardDriveImage, RDB_HEADS, RDB_SPT};
use std::path::Path;

pub use crate::harddrive::SECTOR_SIZE;
/// Maximum sectors per READ/WRITE MULTIPLE block we advertise in IDENTIFY
/// word 47 and accept from SET MULTIPLE.
pub const MAX_MULTIPLE: u8 = 16;

// ATA status bits. BSY is defined for completeness: transfers complete
// within the access in this model, so it is never observable.
#[allow(dead_code)]
const ST_BSY: u8 = 0x80;
const ST_DRDY: u8 = 0x40;
const ST_DSC: u8 = 0x10;
const ST_DRQ: u8 = 0x08;
const ST_ERR: u8 = 0x01;
// ATA error bits.
const ERR_ABRT: u8 = 0x04;
const ERR_IDNF: u8 = 0x10;
// Device control bits.
const CTL_NIEN: u8 = 0x02;
const CTL_SRST: u8 = 0x04;
// Device/head bits.
const DH_LBA: u8 = 0x40;
const DH_DRV: u8 = 0x10;

// Gayle interrupt/status bit layout (shared by the status, interrupt
// change, and interrupt enable registers).
pub const GAYLE_IRQ_IDE: u8 = 0x80;
// PCMCIA bits (CCDET/BVD1/BVD2/WR/BSY) stay clear: no card inserted.

/// IDE register selected by the task-file decode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IdeReg {
    Data,
    ErrorFeature,
    SectorCount,
    SectorNumber,
    CylLow,
    CylHigh,
    DriveHead,
    StatusCommand,
    AltStatusDevCtl,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
enum Transfer {
    None,
    /// Device-to-host PIO (READ SECTORS / READ MULTIPLE / IDENTIFY).
    PioIn {
        /// Sectors still owed after the words currently in the buffer.
        remaining: u32,
        /// Sectors per DRQ block (1, or the SET MULTIPLE count).
        block: u32,
    },
    /// Host-to-device PIO (WRITE SECTORS / WRITE MULTIPLE).
    PioOut {
        remaining: u32,
        block: u32,
    },
}

#[derive(serde::Serialize, serde::Deserialize)]
pub struct IdeDrive {
    /// The sector store (shared with the SCSI targets): HDF file,
    /// directory-built FFS volume, synthesized-RDB overlay handling.
    pub disk: HardDriveImage,
    // Default geometry from the image size; INITIALIZE DEVICE PARAMETERS
    // (0x91) overrides the current translation.
    default_heads: u8,
    default_spt: u8,
    cylinders: u16,
    heads: u8,
    spt: u8,
    multiple: u8,
}

impl IdeDrive {
    /// Open an IDE unit (0 = master, 1 = slave; this picks the DHn device
    /// name a synthesized RDB advertises). The path may be a raw HDF image
    /// file, or a host directory, which is built into an in-memory FFS
    /// volume at open time; `volume_name` labels that volume (directory
    /// mounts only).
    pub fn open(path: &Path, unit: usize, volume_name: Option<&str>) -> anyhow::Result<Self> {
        let disk = HardDriveImage::open(
            path,
            &format!("DH{unit}"),
            "ide",
            "COPPERLINE IDE DISK",
            volume_name,
        )?;
        // The classic Amiga HDF geometry: 16 surfaces, 32 sectors per track
        // (what HDToolBox/RDB tooling defaults to), so the CHS the host
        // computes from an RDB's physical-drive block agrees with what the
        // drive decodes.
        let heads = RDB_HEADS as u8;
        let spt = RDB_SPT as u8;
        let cylinders =
            (disk.total_sectors() / (u64::from(heads) * u64::from(spt))).clamp(1, 65535) as u16;
        Ok(Self {
            disk,
            default_heads: heads,
            default_spt: spt,
            cylinders,
            heads,
            spt,
            multiple: 0,
        })
    }

    /// IDENTIFY DEVICE data. Gayle wires the IDE data bus byte-swapped
    /// relative to the 68000 (IDE D7-D0 land on CPU D15-D8), so the CPU
    /// reads every ATA word with its bytes exchanged. The ROM driver's
    /// scsi.device depends on this: it parses the stored block assuming
    /// PC byte order per word (its word helper at $FB788C and string
    /// helper at $FB7B22 swap each pair back). Sector data is unaffected
    /// because the swap puts file bytes back in natural memory order.
    /// We therefore store each ATA word low-byte-first here, since the
    /// data port read returns `buf[2i] << 8 | buf[2i+1]`.
    fn identify_block(&self) -> Vec<u8> {
        let mut buf = vec![0u8; SECTOR_SIZE];
        let mut word = |idx: usize, val: u16| {
            buf[idx * 2] = (val & 0xFF) as u8;
            buf[idx * 2 + 1] = (val >> 8) as u8;
        };
        // Word 0 mirrors the Conner drives the A600HD shipped with
        // (soft-sectored, fixed, MFM-encoded transfer-rate bits).
        word(0, 0x045A);
        word(1, self.cylinders);
        word(3, u16::from(self.default_heads));
        // ATA-1 unformatted bytes per track/sector: vintage drivers
        // (ROM scsi.device) read these for the block size.
        word(4, u16::from(self.default_spt) * 512);
        word(5, 512);
        word(6, u16::from(self.default_spt));
        word(20, 3); // dual-ported buffer with read caching
        word(21, 64); // buffer size in sectors
        word(22, 4); // ECC bytes for READ/WRITE LONG
        word(48, 1); // can perform doubleword I/O (32-bit host transfers)
        word(51, 0x0200); // PIO data transfer timing mode 2
        word(52, 0x0200); // DMA data transfer timing mode (legacy field)
        word(47, 0x8000 | u16::from(MAX_MULTIPLE));
        word(49, 0x0200); // LBA supported
        word(53, 0x0001); // words 54-58 valid
        word(54, self.cylinders);
        word(55, u16::from(self.heads));
        word(56, u16::from(self.spt));
        let current = u32::from(self.cylinders) * u32::from(self.heads) * u32::from(self.spt);
        word(57, (current & 0xFFFF) as u16);
        word(58, (current >> 16) as u16);
        let lba = self.disk.total_sectors().min(u64::from(u32::MAX)) as u32;
        word(60, (lba & 0xFFFF) as u16);
        word(61, (lba >> 16) as u16);
        word(
            59,
            if self.multiple > 0 {
                0x0100 | u16::from(self.multiple)
            } else {
                0
            },
        );

        // ATA strings carry the first character of each pair in bits 15-8,
        // so with the low-byte-first storage above the pair lands swapped.
        let mut string = |start: usize, len_words: usize, text: &str| {
            let mut bytes = text.as_bytes().to_vec();
            bytes.resize(len_words * 2, b' ');
            for (i, pair) in bytes.chunks(2).enumerate() {
                buf[(start + i) * 2] = pair[1];
                buf[(start + i) * 2 + 1] = pair[0];
            }
        };
        string(10, 10, "CPRLN-0000000000");
        string(23, 4, "1.0 ");
        string(27, 20, "COPPERLINE IDE DISK");
        buf
    }
}

#[derive(serde::Serialize, serde::Deserialize)]
pub struct Gayle {
    /// $DE1000 ID shifted out MSB-first on D7: $D0 (A600) / $D1 (A1200).
    id: u8,
    id_bit: u8,
    /// $DA9000 latched interrupt-change bits (write-to-clear with AND).
    intreq: u8,
    /// $DA9800 interrupt enable.
    intena: u8,
    /// $DAA000 config (PCMCIA voltage/resistor config; stored only).
    config: u8,

    drives: [Option<IdeDrive>; 2],
    // Shared task file (one register file per bus, like the real cable).
    feature: u8,
    error: u8,
    sector_count: u8,
    sector_number: u8,
    cyl_low: u8,
    cyl_high: u8,
    drive_head: u8,
    status: u8,
    devctl: u8,
    intrq: bool,

    buf: Vec<u8>,
    buf_pos: usize,
    transfer: Transfer,
    /// Set whenever the drive does real work (command issued or data port
    /// moved during a transfer); drained by the bus for the HDD LED.
    activity: bool,
}

impl Gayle {
    pub fn new(id: u8) -> Self {
        Self {
            id,
            id_bit: 0,
            intreq: 0,
            intena: 0,
            config: 0,
            drives: [None, None],
            feature: 0,
            error: 0x01, // diagnostics passed
            sector_count: 0x01,
            sector_number: 0x01,
            cyl_low: 0,
            cyl_high: 0,
            drive_head: 0,
            status: ST_DRDY | ST_DSC,
            devctl: 0,
            intrq: false,
            buf: Vec::new(),
            buf_pos: 0,
            transfer: Transfer::None,
            activity: false,
        }
    }

    /// Drain the activity latch set by command issue and data-port traffic.
    /// The bus polls this after each Gayle access to time the HDD LED.
    pub fn take_activity(&mut self) -> bool {
        std::mem::take(&mut self.activity)
    }

    pub fn attach_drive(&mut self, slot: usize, drive: IdeDrive) {
        self.drives[slot.min(1)] = Some(drive);
    }

    /// System reset: clear the register file and any in-flight transfer but
    /// keep the mounted drives.
    pub fn reset(&mut self) {
        self.id_bit = 0;
        self.intreq = 0;
        self.intena = 0;
        self.config = 0;
        self.feature = 0;
        self.sector_count = 0x01;
        self.sector_number = 0x01;
        self.cyl_low = 0;
        self.cyl_high = 0;
        self.drive_head = 0;
        self.devctl = 0;
        self.soft_reset();
    }

    /// The INT2 line into Paula (PORTS): the latched interrupt-change bits
    /// gated by the $DAA000 enable register (the ROM writes $EC there:
    /// IDE plus the PCMCIA detect/change sources). Paula's INTREQ latch is
    /// level-fed, so the bus re-asserts INTREQ.PORTS while this stays true.
    pub fn int2_line(&self) -> bool {
        self.intreq & self.intena != 0
    }

    fn selected(&self) -> usize {
        usize::from(self.drive_head & DH_DRV != 0)
    }

    fn drive(&mut self) -> Option<&mut IdeDrive> {
        self.drives[self.selected()].as_mut()
    }

    fn pair_present(&self) -> bool {
        self.drives[1 - self.selected().min(1)].is_some()
    }

    fn raise_ide_irq(&mut self) {
        self.intrq = true;
        if self.devctl & CTL_NIEN == 0 {
            self.intreq |= GAYLE_IRQ_IDE;
        }
    }

    fn clear_ide_irq(&mut self) {
        self.intrq = false;
    }

    // ----- $DE1000 ID shift register -------------------------------------

    fn id_read(&mut self) -> u8 {
        let bit = (self.id >> (7 - self.id_bit)) & 1;
        self.id_bit = (self.id_bit + 1) & 7;
        if bit != 0 {
            0x80
        } else {
            0x00
        }
    }

    fn id_reset(&mut self) {
        self.id_bit = 0;
    }

    // ----- memory-mapped access ------------------------------------------

    /// Byte/word read anywhere in $DA0000-$DBFFFF or $DE0000-$DEFFFF.
    /// `addr` is the full masked CPU address.
    pub fn read(&mut self, addr: u32, size: usize) -> u32 {
        if size == 4 {
            let hi = self.read(addr, 2);
            let lo = self.read(addr.wrapping_add(2), 2);
            return (hi << 16) | lo;
        }
        let value = self.read_inner(addr, size);
        if crate::envcfg::flag("COPPERLINE_DIAG_GAYLE") {
            log::info!("gayle rd {addr:#08X}/{size} -> {value:#06X}");
        }
        value
    }

    fn read_inner(&mut self, addr: u32, size: usize) -> u32 {
        match (addr, size) {
            (0x00DE_1000..=0x00DE_1003, _) => {
                let v = u32::from(self.id_read());
                // A word read shifts one bit only; it appears on D15-D8.
                if size == 2 {
                    v << 8
                } else {
                    v
                }
            }
            _ if (0x00DA_8000..0x00DB_0000).contains(&addr) => {
                let v = u32::from(self.register_read(addr));
                if size == 2 {
                    v << 8
                } else {
                    v
                }
            }
            _ if (0x00DA_0000..0x00DA_8000).contains(&addr) => self.ide_read(addr, size),
            _ => 0,
        }
    }

    pub fn write(&mut self, addr: u32, size: usize, value: u32) {
        if size == 4 {
            self.write(addr, 2, value >> 16);
            self.write(addr.wrapping_add(2), 2, value & 0xFFFF);
            return;
        }
        if crate::envcfg::flag("COPPERLINE_DIAG_GAYLE") {
            log::info!("gayle wr {addr:#08X}/{size} <- {value:#06X}");
        }
        match addr {
            0x00DE_1000..=0x00DE_1003 => self.id_reset(),
            _ if (0x00DA_8000..0x00DB_0000).contains(&addr) => {
                let byte = if size == 2 {
                    (value >> 8) as u8
                } else {
                    value as u8
                };
                self.register_write(addr, byte);
            }
            _ if (0x00DA_0000..0x00DA_8000).contains(&addr) => self.ide_write(addr, size, value),
            _ => {}
        }
    }

    fn register_read(&mut self, addr: u32) -> u8 {
        match addr & 0xFFFF_F000 {
            0x00DA_8000 => {
                // Status: live IDE INTRQ on bit 7. The PCMCIA pins are
                // active-low and pulled up, so an EMPTY slot reads with the
                // card-detect/battery/write/busy bits SET (0x7C); all-zero
                // would tell card.resource a card is inserted and wedge boot
                // waiting for it to become ready.
                let pcmcia_empty = 0x7C;
                if self.intrq && self.devctl & CTL_NIEN == 0 {
                    GAYLE_IRQ_IDE | pcmcia_empty
                } else {
                    pcmcia_empty
                }
            }
            0x00DA_9000 => self.intreq,
            0x00DA_A000 => self.intena,
            0x00DA_B000 => self.config,
            _ => 0,
        }
    }

    fn register_write(&mut self, addr: u32, value: u8) {
        match addr & 0xFFFF_F000 {
            0x00DA_8000 => {
                // Status register writes only touch the PCMCIA control bits;
                // nothing modeled behind them with an empty slot.
            }
            0x00DA_9000 => {
                // Interrupt change: write-to-clear. Bits written as 1 are
                // kept, bits written as 0 are cleared.
                self.intreq &= value;
            }
            0x00DA_A000 => self.intena = value,
            0x00DA_B000 => self.config = value,
            _ => {}
        }
    }

    // ----- IDE task file ---------------------------------------------------

    /// A600/A1200 IDE decode, as the ROM scsi.device drives it (verified
    /// against ROM 40.063 boot probes): task file at $DA2000 with a 4-byte
    /// stride, byte registers on the even (D15-D8) byte, the 16-bit data
    /// port at $DA2000, and the control block one A12 page up ($DA3018).
    fn ide_reg(addr: u32, write: bool) -> Option<IdeReg> {
        let off = addr & 0x7FFF;
        Some(match off {
            0x2000 | 0x2002 => IdeReg::Data,
            0x2004 | 0x2006 => IdeReg::ErrorFeature,
            0x2008 | 0x200A => IdeReg::SectorCount,
            0x200C | 0x200E => IdeReg::SectorNumber,
            0x2010 | 0x2012 => IdeReg::CylLow,
            0x2014 | 0x2016 => IdeReg::CylHigh,
            0x2018 | 0x201A => IdeReg::DriveHead,
            0x201C | 0x201E => IdeReg::StatusCommand,
            0x3018 | 0x301A => IdeReg::AltStatusDevCtl,
            _ => {
                let _ = write;
                return None;
            }
        })
    }

    fn ide_read(&mut self, addr: u32, size: usize) -> u32 {
        // Selected device absent: the status register reads 0x01 (ERR set,
        // not ready) when the other device is present and 0xFF when the
        // cable is empty; every other task-file register reads zero, and a
        // status read drops a pending interrupt (the INTRQ line is shared).
        // This is how the ROM probe concludes a unit does not exist
        // instead of classifying it as a pre-ATA drive (matches WinUAE).
        if self.drives[self.selected()].is_none() {
            return match Self::ide_reg(addr, false) {
                Some(IdeReg::StatusCommand) | Some(IdeReg::AltStatusDevCtl) => {
                    self.clear_ide_irq();
                    if self.pair_present() {
                        0x01
                    } else {
                        0xFF
                    }
                }
                _ => 0,
            };
        }
        match Self::ide_reg(addr, false) {
            Some(IdeReg::Data) => {
                let word = self.data_read_word();
                if size == 1 {
                    u32::from(word >> 8)
                } else {
                    u32::from(word)
                }
            }
            Some(IdeReg::ErrorFeature) => u32::from(self.error),
            Some(IdeReg::SectorCount) => u32::from(self.sector_count),
            Some(IdeReg::SectorNumber) => u32::from(self.sector_number),
            Some(IdeReg::CylLow) => u32::from(self.cyl_low),
            Some(IdeReg::CylHigh) => u32::from(self.cyl_high),
            Some(IdeReg::DriveHead) => u32::from(self.drive_head),
            Some(IdeReg::StatusCommand) => {
                let v = self.status_read();
                self.clear_ide_irq();
                u32::from(v)
            }
            Some(IdeReg::AltStatusDevCtl) => u32::from(self.status_read()),
            None => 0,
        }
    }

    fn ide_write(&mut self, addr: u32, size: usize, value: u32) {
        let byte = value as u8;
        match Self::ide_reg(addr, true) {
            Some(IdeReg::Data) => {
                let word = if size == 1 {
                    (value as u16) << 8
                } else {
                    value as u16
                };
                self.data_write_word(word);
            }
            Some(IdeReg::ErrorFeature) => self.feature = byte,
            Some(IdeReg::SectorCount) => self.sector_count = byte,
            Some(IdeReg::SectorNumber) => self.sector_number = byte,
            Some(IdeReg::CylLow) => self.cyl_low = byte,
            Some(IdeReg::CylHigh) => self.cyl_high = byte,
            Some(IdeReg::DriveHead) => self.drive_head = byte,
            Some(IdeReg::StatusCommand) => self.command(byte),
            Some(IdeReg::AltStatusDevCtl) => {
                let was_reset = self.devctl & CTL_SRST != 0;
                self.devctl = byte;
                if byte & CTL_SRST != 0 && !was_reset {
                    self.soft_reset();
                }
            }
            None => {}
        }
    }

    fn status_read(&self) -> u8 {
        self.status
    }

    fn soft_reset(&mut self) {
        self.status = ST_DRDY | ST_DSC;
        self.error = 0x01;
        self.transfer = Transfer::None;
        self.buf.clear();
        self.buf_pos = 0;
        self.clear_ide_irq();
    }

    // ----- data port -------------------------------------------------------

    fn data_read_word(&mut self) -> u16 {
        if !matches!(self.transfer, Transfer::PioIn { .. }) || self.buf_pos + 1 >= self.buf.len() {
            return 0;
        }
        let word = (u16::from(self.buf[self.buf_pos]) << 8) | u16::from(self.buf[self.buf_pos + 1]);
        self.buf_pos += 2;
        self.activity = true;
        if self.buf_pos >= self.buf.len() {
            self.pio_in_block_consumed();
        }
        word
    }

    fn data_write_word(&mut self, word: u16) {
        if !matches!(self.transfer, Transfer::PioOut { .. }) || self.buf_pos + 1 >= self.buf.len() {
            return;
        }
        self.buf[self.buf_pos] = (word >> 8) as u8;
        self.buf[self.buf_pos + 1] = (word & 0xFF) as u8;
        self.buf_pos += 2;
        self.activity = true;
        if self.buf_pos >= self.buf.len() {
            self.pio_out_block_filled();
        }
    }

    fn pio_in_block_consumed(&mut self) {
        let Transfer::PioIn { remaining, block } = self.transfer else {
            // IDENTIFY-style single buffer: transfer complete.
            self.status = ST_DRDY | ST_DSC;
            self.transfer = Transfer::None;
            return;
        };
        if remaining == 0 {
            self.status = ST_DRDY | ST_DSC;
            self.transfer = Transfer::None;
            return;
        }
        let chunk = remaining.min(block);
        if self.fill_read_buffer(chunk).is_ok() {
            self.transfer = Transfer::PioIn {
                remaining: remaining - chunk,
                block,
            };
            self.status = ST_DRDY | ST_DSC | ST_DRQ;
            self.raise_ide_irq();
        }
    }

    fn pio_out_block_filled(&mut self) {
        let Transfer::PioOut { remaining, block } = self.transfer else {
            return;
        };
        // Commit the buffered sectors at the current task-file position.
        if self.commit_write_buffer().is_err() {
            return;
        }
        if remaining == 0 {
            if let Some(drive) = self.drive() {
                drive.disk.flush();
            }
            self.status = ST_DRDY | ST_DSC;
            self.transfer = Transfer::None;
            self.raise_ide_irq();
            return;
        }
        let chunk = remaining.min(block);
        self.buf.clear();
        self.buf.resize(chunk as usize * SECTOR_SIZE, 0);
        self.buf_pos = 0;
        self.transfer = Transfer::PioOut {
            remaining: remaining - chunk,
            block,
        };
        self.status = ST_DRDY | ST_DSC | ST_DRQ;
        self.raise_ide_irq();
    }

    // ----- addressing -------------------------------------------------------

    /// Current LBA from the task file (LBA28 or CHS translation).
    fn current_lba(&mut self) -> Option<u64> {
        let lba_mode = self.drive_head & DH_LBA != 0;
        let head = u64::from(self.drive_head & 0x0F);
        let sector = u64::from(self.sector_number);
        let cyl = (u64::from(self.cyl_high) << 8) | u64::from(self.cyl_low);
        let drive = self.drive()?;
        if lba_mode {
            Some((head << 24) | (cyl << 8) | sector)
        } else {
            if sector == 0 {
                return None;
            }
            let heads = u64::from(drive.heads);
            let spt = u64::from(drive.spt);
            Some((cyl * heads + head) * spt + (sector - 1))
        }
    }

    /// Advance the task-file position by one sector, as real drives do, so
    /// software can resume after a partial transfer.
    fn advance_lba(&mut self) {
        if self.drive_head & DH_LBA != 0 {
            let lba = ((u32::from(self.drive_head & 0x0F) << 24)
                | (u32::from(self.cyl_high) << 16)
                | (u32::from(self.cyl_low) << 8)
                | u32::from(self.sector_number))
            .wrapping_add(1);
            self.sector_number = (lba & 0xFF) as u8;
            self.cyl_low = ((lba >> 8) & 0xFF) as u8;
            self.cyl_high = ((lba >> 16) & 0xFF) as u8;
            self.drive_head = (self.drive_head & 0xF0) | ((lba >> 24) & 0x0F) as u8;
            return;
        }
        let (heads, spt) = match self.drive() {
            Some(d) => (d.heads, d.spt),
            None => return,
        };
        if self.sector_number < spt {
            self.sector_number += 1;
            return;
        }
        self.sector_number = 1;
        let head = self.drive_head & 0x0F;
        if head + 1 < heads {
            self.drive_head = (self.drive_head & 0xF0) | (head + 1);
            return;
        }
        self.drive_head &= 0xF0;
        let cyl = ((u16::from(self.cyl_high) << 8) | u16::from(self.cyl_low)).wrapping_add(1);
        self.cyl_low = (cyl & 0xFF) as u8;
        self.cyl_high = (cyl >> 8) as u8;
    }

    fn fill_read_buffer(&mut self, sectors: u32) -> Result<(), ()> {
        self.buf.clear();
        self.buf_pos = 0;
        for _ in 0..sectors {
            let Some(lba) = self.current_lba() else {
                self.command_error(ERR_IDNF);
                return Err(());
            };
            let total = self.drive().map(|d| d.disk.total_sectors()).unwrap_or(0);
            if lba >= total {
                self.command_error(ERR_IDNF);
                return Err(());
            }
            let mut sector = [0u8; SECTOR_SIZE];
            let res = self
                .drive()
                .map(|d| d.disk.read_sector(lba, &mut sector))
                .unwrap_or_else(|| Err(std::io::ErrorKind::NotFound.into()));
            if let Err(e) = res {
                log::warn!("IDE read lba {lba}: {e}");
                self.command_error(ERR_ABRT);
                return Err(());
            }
            self.buf.extend_from_slice(&sector);
            self.advance_lba();
        }
        Ok(())
    }

    fn commit_write_buffer(&mut self) -> Result<(), ()> {
        let sectors = self.buf.len() / SECTOR_SIZE;
        for i in 0..sectors {
            let Some(lba) = self.current_lba() else {
                self.command_error(ERR_IDNF);
                return Err(());
            };
            let total = self.drive().map(|d| d.disk.total_sectors()).unwrap_or(0);
            if lba >= total {
                self.command_error(ERR_IDNF);
                return Err(());
            }
            let start = i * SECTOR_SIZE;
            let sector: [u8; SECTOR_SIZE] =
                self.buf[start..start + SECTOR_SIZE].try_into().unwrap();
            let res = self
                .drive()
                .map(|d| d.disk.write_sector(lba, &sector))
                .unwrap_or_else(|| Err(std::io::ErrorKind::NotFound.into()));
            if let Err(e) = res {
                log::warn!("IDE write lba {lba}: {e}");
                self.command_error(ERR_ABRT);
                return Err(());
            }
            self.advance_lba();
        }
        Ok(())
    }

    fn command_error(&mut self, error_bits: u8) {
        self.error = error_bits;
        self.status = ST_DRDY | ST_DSC | ST_ERR;
        self.transfer = Transfer::None;
        self.buf.clear();
        self.buf_pos = 0;
        self.raise_ide_irq();
    }

    // ----- command dispatch --------------------------------------------------

    fn command(&mut self, cmd: u8) {
        if crate::envcfg::flag("COPPERLINE_DIAG_GAYLE") {
            let lba = self.drive_head & DH_LBA != 0;
            log::info!(
                "ide cmd {cmd:#04X} drv={} lba={} chs/lba=({:02X} {:02X} {:02X} {:02X}) n={}",
                self.selected(),
                lba,
                self.drive_head & 0x0F,
                self.cyl_high,
                self.cyl_low,
                self.sector_number,
                self.sector_count
            );
        }
        self.clear_ide_irq();
        if self.drives[self.selected()].is_none() {
            // Every command addressed to an absent device fails with
            // command-aborted and raises the completion interrupt, so the
            // host's probe finishes promptly (matches WinUAE; the ROM's
            // INITIALIZE DEVICE PARAMETERS arrives with the DEV bit set
            // and must complete one way or the other).
            self.command_error(ERR_ABRT);
            return;
        }
        self.error = 0;
        self.status = ST_DRDY | ST_DSC;
        self.activity = true;
        let count = if self.sector_count == 0 {
            256u32
        } else {
            u32::from(self.sector_count)
        };
        match cmd {
            // IDENTIFY DEVICE
            0xEC => {
                self.buf = self.drive().map(|d| d.identify_block()).unwrap_or_default();
                self.buf_pos = 0;
                self.transfer = Transfer::PioIn {
                    remaining: 0,
                    block: 1,
                };
                self.status = ST_DRDY | ST_DSC | ST_DRQ;
                self.raise_ide_irq();
            }
            // READ SECTORS (with/without retry) and READ MULTIPLE.
            0x20 | 0x21 | 0xC4 => {
                let block = if cmd == 0xC4 {
                    let m = self.drive().map(|d| d.multiple).unwrap_or(0);
                    if m == 0 {
                        self.command_error(ERR_ABRT);
                        return;
                    }
                    u32::from(m)
                } else {
                    1
                };
                let chunk = count.min(block);
                self.transfer = Transfer::PioIn {
                    remaining: count - chunk,
                    block,
                };
                if self.fill_read_buffer(chunk).is_ok() {
                    self.status = ST_DRDY | ST_DSC | ST_DRQ;
                    self.raise_ide_irq();
                }
            }
            // WRITE SECTORS (with/without retry) and WRITE MULTIPLE.
            0x30 | 0x31 | 0xC5 => {
                let block = if cmd == 0xC5 {
                    let m = self.drive().map(|d| d.multiple).unwrap_or(0);
                    if m == 0 {
                        self.command_error(ERR_ABRT);
                        return;
                    }
                    u32::from(m)
                } else {
                    1
                };
                let chunk = count.min(block);
                self.buf.clear();
                self.buf.resize(chunk as usize * SECTOR_SIZE, 0);
                self.buf_pos = 0;
                self.transfer = Transfer::PioOut {
                    remaining: count - chunk,
                    block,
                };
                // First DRQ block is ready without an interrupt (ATA PIO out).
                self.status = ST_DRDY | ST_DSC | ST_DRQ;
            }
            // SET MULTIPLE MODE
            0xC6 => {
                let requested = self.sector_count;
                let ok =
                    requested <= MAX_MULTIPLE && (requested == 0 || requested.is_power_of_two());
                if let (true, Some(drive)) = (ok, self.drive()) {
                    drive.multiple = requested;
                    self.status = ST_DRDY | ST_DSC;
                    self.raise_ide_irq();
                } else {
                    self.command_error(ERR_ABRT);
                }
            }
            // INITIALIZE DEVICE PARAMETERS: set current CHS translation.
            // A zero sector count is invalid and aborts, as on real drives.
            0x91 => {
                let heads = (self.drive_head & 0x0F) + 1;
                let spt = self.sector_count;
                if spt == 0 {
                    self.command_error(ERR_ABRT);
                    return;
                }
                if let Some(drive) = self.drive() {
                    drive.heads = heads;
                    drive.spt = spt;
                    let total = drive.disk.total_sectors();
                    drive.cylinders =
                        (total / (u64::from(heads) * u64::from(spt)).max(1)).clamp(1, 65535) as u16;
                }
                self.status = ST_DRDY | ST_DSC;
                self.raise_ide_irq();
            }
            // RECALIBRATE
            0x10..=0x1F => {
                self.status = ST_DRDY | ST_DSC;
                self.raise_ide_irq();
            }
            // NOP: per ATA-2 always aborts.
            0x00 => self.command_error(ERR_ABRT),
            _ => {
                log::warn!("IDE: unimplemented command {cmd:#04X}");
                self.command_error(ERR_ABRT);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harddrive::CYL_SECTORS;
    use std::path::PathBuf;

    fn temp_image(sectors: u64) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "copperline-gayle-test-{}-{}.hdf",
            std::process::id(),
            rand_suffix()
        ));
        let data = vec![0u8; (sectors * SECTOR_SIZE as u64) as usize];
        std::fs::write(&path, data).unwrap();
        path
    }

    fn rand_suffix() -> u64 {
        use std::sync::atomic::{AtomicU64, Ordering};
        use std::time::{SystemTime, UNIX_EPOCH};
        // Parallel tests can hit the same nanosecond timestamp; a
        // process-wide counter keeps the image paths distinct.
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .subsec_nanos() as u64;
        (nanos << 16) | NEXT.fetch_add(1, Ordering::Relaxed)
    }

    fn gayle_with_drive(sectors: u64) -> (Gayle, PathBuf) {
        let path = temp_image(sectors);
        let mut gayle = Gayle::new(0xD0);
        gayle.attach_drive(0, IdeDrive::open(&path, 0, None).unwrap());
        (gayle, path)
    }

    const IDE_DATA: u32 = 0x00DA_2000;
    const IDE_ERROR: u32 = 0x00DA_2004;
    const IDE_NSECTOR: u32 = 0x00DA_2008;
    const IDE_SECTOR: u32 = 0x00DA_200C;
    const IDE_LCYL: u32 = 0x00DA_2010;
    const IDE_HCYL: u32 = 0x00DA_2014;
    const IDE_SELECT: u32 = 0x00DA_2018;
    const IDE_STATUS: u32 = 0x00DA_201C;
    const GAYLE_INTREQ: u32 = 0x00DA_9000;
    const GAYLE_INTENA: u32 = 0x00DA_A000;
    const GAYLE_STATUS_REG: u32 = 0x00DA_8000;
    const GAYLE_ID_REG: u32 = 0x00DE_1000;

    fn set_lba(g: &mut Gayle, lba: u32, count: u8) {
        g.write(
            IDE_SELECT,
            1,
            u32::from(DH_LBA | ((lba >> 24) as u8 & 0x0F)),
        );
        g.write(IDE_HCYL, 1, (lba >> 16) & 0xFF);
        g.write(IDE_LCYL, 1, (lba >> 8) & 0xFF);
        g.write(IDE_SECTOR, 1, lba & 0xFF);
        g.write(IDE_NSECTOR, 1, u32::from(count));
    }

    fn be32(block: &[u8], offset: usize) -> u32 {
        u32::from_be_bytes(block[offset..offset + 4].try_into().unwrap())
    }

    fn rdb_block_sums_to_zero(block: &[u8]) -> bool {
        (0..64)
            .map(|i| be32(block, i * 4))
            .fold(0u32, |a, v| a.wrapping_add(v))
            == 0
    }

    #[test]
    fn bare_partition_hardfile_gets_synthesized_rdb() {
        // One cylinder (256 KiB) of FFS partition: boot block 'DOS\x03'.
        let path = temp_image(CYL_SECTORS as u64);
        let mut data = std::fs::read(&path).unwrap();
        data[..4].copy_from_slice(b"DOS\x03");
        data[SECTOR_SIZE] = 0xA5; // marker in partition sector 1
        std::fs::write(&path, &data).unwrap();

        let mut drive = IdeDrive::open(&path, 0, None).unwrap();
        // One synthesized RDB cylinder plus the partition cylinder.
        assert_eq!(drive.disk.total_sectors(), 2 * u64::from(CYL_SECTORS));

        let mut sector = [0u8; SECTOR_SIZE];
        drive.disk.read_sector(0, &mut sector).unwrap();
        assert_eq!(&sector[..4], b"RDSK");
        assert!(rdb_block_sums_to_zero(&sector));
        assert_eq!(be32(&sector, 64), 2); // cylinders
        assert_eq!(be32(&sector, 68), RDB_SPT);
        assert_eq!(be32(&sector, 72), RDB_HEADS);

        drive.disk.read_sector(1, &mut sector).unwrap();
        assert_eq!(&sector[..4], b"PART");
        assert!(rdb_block_sums_to_zero(&sector));
        assert_eq!(&sector[36..40], b"\x03DH0"); // BSTR drive name
        assert_eq!(be32(&sector, 128 + 9 * 4), 1); // low cylinder
        assert_eq!(be32(&sector, 128 + 10 * 4), 1); // high cylinder
        assert_eq!(be32(&sector, 128 + 16 * 4), 0x444F_5303); // dostype DOS\x03

        // Partition LBAs shift down one cylinder onto the file.
        drive
            .disk
            .read_sector(u64::from(CYL_SECTORS), &mut sector)
            .unwrap();
        assert_eq!(&sector[..4], b"DOS\x03");
        drive
            .disk
            .read_sector(u64::from(CYL_SECTORS) + 1, &mut sector)
            .unwrap();
        assert_eq!(sector[0], 0xA5);

        // Writes to the partition persist in the file at the shifted offset;
        // writes to the synthesized RDB stay in memory.
        let mut payload = [0u8; SECTOR_SIZE];
        payload[..4].copy_from_slice(b"WRIT");
        drive
            .disk
            .write_sector(u64::from(CYL_SECTORS) + 2, &payload)
            .unwrap();
        drive.disk.write_sector(0, &payload).unwrap();
        drive.disk.read_sector(0, &mut sector).unwrap();
        assert_eq!(&sector[..4], b"WRIT");
        let on_disk = std::fs::read(&path).unwrap();
        assert_eq!(&on_disk[2 * SECTOR_SIZE..2 * SECTOR_SIZE + 4], b"WRIT");
        assert_eq!(&on_disk[..4], b"DOS\x03"); // RDB write did not hit the file

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn image_with_own_rdsk_is_not_wrapped() {
        let path = temp_image(CYL_SECTORS as u64);
        let mut data = std::fs::read(&path).unwrap();
        data[..4].copy_from_slice(b"RDSK");
        std::fs::write(&path, &data).unwrap();
        let mut drive = IdeDrive::open(&path, 0, None).unwrap();
        assert_eq!(drive.disk.total_sectors(), u64::from(CYL_SECTORS));
        let mut sector = [0u8; SECTOR_SIZE];
        drive.disk.read_sector(0, &mut sector).unwrap();
        assert_eq!(&sector[..4], b"RDSK");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn bare_partition_with_uneven_size_is_rejected() {
        // Half a cylinder: detected as a bare partition but not wrappable.
        let path = temp_image(u64::from(CYL_SECTORS) / 2);
        let mut data = std::fs::read(&path).unwrap();
        data[..4].copy_from_slice(b"DOS\x00");
        std::fs::write(&path, &data).unwrap();
        let err = match IdeDrive::open(&path, 0, None) {
            Ok(_) => panic!("expected open to fail"),
            Err(e) => e.to_string(),
        };
        assert!(err.contains("bare partition"), "unexpected error: {err}");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn gayle_id_shifts_out_msb_first_on_d7() {
        let mut gayle = Gayle::new(0xD0);
        gayle.write(GAYLE_ID_REG, 1, 0xFF); // any write resets the shifter
        let bits: Vec<u32> = (0..8).map(|_| gayle.read(GAYLE_ID_REG, 1)).collect();
        // 0xD0 = 1101 0000.
        assert_eq!(bits, [0x80, 0x80, 0x00, 0x80, 0x00, 0x00, 0x00, 0x00]);
        // A fresh write restarts the sequence.
        gayle.write(GAYLE_ID_REG, 1, 0x00);
        assert_eq!(gayle.read(GAYLE_ID_REG, 1), 0x80);

        let mut a1200 = Gayle::new(0xD1);
        a1200.write(GAYLE_ID_REG, 1, 0);
        let bits: Vec<u32> = (0..8).map(|_| a1200.read(GAYLE_ID_REG, 1)).collect();
        assert_eq!(bits, [0x80, 0x80, 0x00, 0x80, 0x00, 0x00, 0x00, 0x80]);
    }

    #[test]
    fn identify_reports_geometry_lba_and_multiple() {
        let (mut g, path) = gayle_with_drive(16 * 32 * 4); // 4 cylinders
        g.write(IDE_SELECT, 1, 0xA0);
        g.write(IDE_STATUS, 1, 0xEC);
        let status = g.read(0x00DA_3018, 1); // alt status: no irq clear
        assert_eq!(status as u8, ST_DRDY | ST_DSC | ST_DRQ);
        g.write(GAYLE_INTENA, 1, u32::from(GAYLE_IRQ_IDE));
        assert!(g.int2_line());

        // The CPU sees every ATA word byte-swapped (Gayle's IDE data bus
        // wiring); undo the swap to check the ATA-defined values.
        let mut words = [0u16; 256];
        for w in words.iter_mut() {
            *w = (g.read(IDE_DATA, 2) as u16).swap_bytes();
        }
        assert_eq!(words[0], 0x045A, "Conner-style configuration word");
        assert_eq!(words[1], 4, "cylinders");
        assert_eq!(words[3], 16, "heads");
        assert_eq!(words[6], 32, "sectors per track");
        assert_eq!(words[47] & 0xFF, u16::from(MAX_MULTIPLE));
        assert_ne!(words[49] & 0x0200, 0, "LBA capability");
        let lba = u32::from(words[60]) | (u32::from(words[61]) << 16);
        assert_eq!(lba, 16 * 32 * 4);
        // ATA string convention: first char of each pair in bits 15-8.
        assert_eq!(words[27], u16::from_be_bytes([b'C', b'O']));
        // Transfer complete: DRQ clears.
        assert_eq!(g.read(IDE_STATUS, 1) as u8, ST_DRDY | ST_DSC);
        std::fs::remove_file(path).ok();
    }

    #[test]
    fn write_then_read_sectors_round_trips_through_the_image() {
        let (mut g, path) = gayle_with_drive(64);
        g.write(GAYLE_INTENA, 1, u32::from(GAYLE_IRQ_IDE));

        // WRITE SECTORS, 2 sectors at LBA 5.
        set_lba(&mut g, 5, 2);
        g.write(IDE_STATUS, 1, 0x30);
        assert_eq!(
            g.read(0x00DA_3018, 1) as u8,
            ST_DRDY | ST_DSC | ST_DRQ,
            "first DRQ block ready without IRQ"
        );
        assert!(!g.int2_line(), "no IRQ before first block is consumed");
        for i in 0..512u32 {
            g.write(IDE_DATA, 2, (i * 7) & 0xFFFF);
        }
        assert_eq!(g.read(IDE_STATUS, 1) as u8, ST_DRDY | ST_DSC);

        // READ SECTORS back.
        set_lba(&mut g, 5, 2);
        g.write(IDE_STATUS, 1, 0x20);
        assert!(g.int2_line(), "read data ready raises INT2");
        let mut got = Vec::with_capacity(512);
        for _ in 0..512 {
            got.push(g.read(IDE_DATA, 2) as u16);
        }
        for (i, w) in got.iter().enumerate() {
            assert_eq!(u32::from(*w), (i as u32 * 7) & 0xFFFF, "word {i}");
        }
        assert_eq!(g.read(IDE_STATUS, 1) as u8, ST_DRDY | ST_DSC);

        // The bytes really hit the backing file (big-endian word order).
        let data = std::fs::read(&path).unwrap();
        let off = 5 * SECTOR_SIZE;
        assert_eq!(data[off], 0);
        assert_eq!(data[off + 1], 0);
        assert_eq!(data[off + 2], 0);
        assert_eq!(data[off + 3], 7);
        std::fs::remove_file(path).ok();
    }

    #[test]
    fn set_multiple_and_read_multiple_transfer_in_blocks() {
        let (mut g, path) = gayle_with_drive(64);
        g.write(GAYLE_INTENA, 1, u32::from(GAYLE_IRQ_IDE));

        // SET MULTIPLE = 4.
        g.write(IDE_NSECTOR, 1, 4);
        g.write(IDE_STATUS, 1, 0xC6);
        assert_eq!(g.read(IDE_STATUS, 1) as u8, ST_DRDY | ST_DSC);

        // READ MULTIPLE of 8 sectors: expect 2 DRQ blocks of 4.
        set_lba(&mut g, 0, 8);
        g.write(IDE_STATUS, 1, 0xC4);
        let mut blocks = 0;
        while g.read(0x00DA_3018, 1) as u8 & ST_DRQ != 0 {
            blocks += 1;
            assert!(blocks <= 2, "expected exactly two DRQ blocks");
            for _ in 0..(4 * 256) {
                g.read(IDE_DATA, 2);
            }
        }
        assert_eq!(blocks, 2);

        // SET MULTIPLE beyond the advertised maximum aborts.
        g.write(IDE_NSECTOR, 1, 64);
        g.write(IDE_STATUS, 1, 0xC6);
        assert_ne!(g.read(IDE_STATUS, 1) as u8 & ST_ERR, 0);
        assert_ne!(g.read(IDE_ERROR, 1) as u8 & ERR_ABRT, 0);
        std::fs::remove_file(path).ok();
    }

    #[test]
    fn chs_addressing_follows_initialize_device_parameters() {
        let (mut g, path) = gayle_with_drive(64);
        // INITIALIZE DEVICE PARAMETERS: 2 heads, 8 sectors per track.
        g.write(IDE_SELECT, 1, 0xA0 | 1); // heads - 1
        g.write(IDE_NSECTOR, 1, 8);
        g.write(IDE_STATUS, 1, 0x91);
        assert_eq!(g.read(IDE_STATUS, 1) as u8, ST_DRDY | ST_DSC);

        // Write one sector at C/H/S = 1/1/3 -> LBA (1*2+1)*8 + 2 = 26.
        g.write(IDE_SELECT, 1, 0xA0 | 1);
        g.write(IDE_HCYL, 1, 0);
        g.write(IDE_LCYL, 1, 1);
        g.write(IDE_SECTOR, 1, 3);
        g.write(IDE_NSECTOR, 1, 1);
        g.write(IDE_STATUS, 1, 0x30);
        for i in 0..256u32 {
            g.write(IDE_DATA, 2, 0xBEE0 + (i & 0xF));
        }
        assert_eq!(g.read(IDE_STATUS, 1) as u8, ST_DRDY | ST_DSC);
        let data = std::fs::read(&path).unwrap();
        let off = 26 * SECTOR_SIZE;
        assert_eq!(data[off], 0xBE);
        assert_eq!(data[off + 1], 0xE0);
        std::fs::remove_file(path).ok();
    }

    #[test]
    fn gayle_interrupt_latch_is_write_to_clear_and_gates_int2() {
        let (mut g, path) = gayle_with_drive(64);
        // The latch records the IRQ regardless; the $DAA000 enable gates
        // its delivery to INT2.
        set_lba(&mut g, 0, 1);
        g.write(IDE_STATUS, 1, 0x20);
        assert_eq!(g.read(GAYLE_INTREQ, 1) as u8 & GAYLE_IRQ_IDE, GAYLE_IRQ_IDE);
        assert!(!g.int2_line(), "INTENA clear blocks INT2");
        g.write(GAYLE_INTENA, 1, u32::from(GAYLE_IRQ_IDE));
        assert!(g.int2_line());
        // Live INTRQ shows in the status register.
        assert_eq!(
            g.read(GAYLE_STATUS_REG, 1) as u8 & GAYLE_IRQ_IDE,
            GAYLE_IRQ_IDE
        );
        // Write-to-clear: writing 0 to bit 7 clears the latch.
        g.write(GAYLE_INTREQ, 1, u32::from(!GAYLE_IRQ_IDE));
        assert_eq!(g.read(GAYLE_INTREQ, 1) as u8 & GAYLE_IRQ_IDE, 0);
        assert!(!g.int2_line());
        std::fs::remove_file(path).ok();
    }

    #[test]
    fn missing_device_status_follows_winuae_pair_semantics() {
        // Empty cable: every status read floats to 0xFF.
        let mut g = Gayle::new(0xD0);
        g.write(IDE_SELECT, 1, 0xB0);
        assert_eq!(g.read(IDE_STATUS, 1) as u8, 0xFF, "empty cable floats");
        assert_eq!(g.read(IDE_ERROR, 1), 0, "non-status registers read 0");

        // Master present, slave selected: status reads 0x01, commands abort.
        let (mut g, path) = gayle_with_drive(64);
        g.write(GAYLE_INTENA, 1, u32::from(GAYLE_IRQ_IDE));
        g.write(IDE_SELECT, 1, 0xB0);
        assert_eq!(g.read(IDE_STATUS, 1) as u8, 0x01, "pair present");
        assert_eq!(g.read(IDE_ERROR, 1), 0, "non-status registers read 0");
        g.write(IDE_STATUS, 1, 0xEC);
        assert!(g.int2_line(), "aborted command still raises the IRQ");
        assert_eq!(
            g.read(IDE_STATUS, 1) as u8,
            0x01,
            "no phantom IDENTIFY: status stays at the pair-present pattern"
        );
        std::fs::remove_file(path).ok();
    }
}
