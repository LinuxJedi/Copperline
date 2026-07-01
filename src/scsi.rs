// SPDX-License-Identifier: GPL-3.0-or-later

//! WD33C93A SCSI bus interface controller ("SBIC") and the SCSI-2
//! direct-access disk targets behind it.
//!
//! The chip model follows the WD33C93A datasheet as exercised by the
//! Commodore A2091/A590 boot-ROM driver: the register file is reached
//! through the address pointer (SASR) and the data port, and the driver
//! runs the bus almost entirely through the Select-and-Transfer
//! combination command with DMA data phases handled by the board's DMAC.
//! The manual path (Select + Transfer Info per phase) is modeled too for
//! drivers that step phases themselves.
//!
//! Transfers complete within the access in this model (like the Gayle IDE
//! model); completion interrupts are delivered after a short emulated
//! delay so software that issues a command and then polls the auxiliary
//! status register observes the busy-then-interrupt sequence.

use crate::harddrive::{HardDriveImage, SECTOR_SIZE};
use std::collections::VecDeque;
use std::path::Path;

// ----- WD33C93A register file (SASR values) --------------------------------

pub const WD_OWN_ID: u8 = 0x00;
pub const WD_CONTROL: u8 = 0x01;
pub const WD_TIMEOUT: u8 = 0x02;
/// CDB bytes occupy 0x03-0x0E.
pub const WD_CDB_1: u8 = 0x03;
/// Target LUN before a command; the target's status byte after a completed
/// Select-and-Transfer.
pub const WD_TARGET_LUN: u8 = 0x0F;
pub const WD_COMMAND_PHASE: u8 = 0x10;
pub const WD_TC_MSB: u8 = 0x12;
pub const WD_TC_MID: u8 = 0x13;
pub const WD_TC_LSB: u8 = 0x14;
pub const WD_DESTINATION_ID: u8 = 0x15;
pub const WD_SCSI_STATUS: u8 = 0x17;
pub const WD_COMMAND: u8 = 0x18;
pub const WD_DATA: u8 = 0x19;

// ----- auxiliary status bits ------------------------------------------------

pub const ASR_INT: u8 = 0x80;
pub const ASR_BSY: u8 = 0x20;
pub const ASR_CIP: u8 = 0x10;
pub const ASR_DBR: u8 = 0x01;

// ----- chip commands ---------------------------------------------------------

const CMD_RESET: u8 = 0x00;
const CMD_ABORT: u8 = 0x01;
const CMD_ASSERT_ATN: u8 = 0x02;
const CMD_NEGATE_ACK: u8 = 0x03;
const CMD_DISCONNECT: u8 = 0x04;
const CMD_SELECT_ATN: u8 = 0x06;
const CMD_SELECT: u8 = 0x07;
const CMD_SELECT_ATN_XFER: u8 = 0x08;
const CMD_SELECT_XFER: u8 = 0x09;
const CMD_TRANSFER_INFO: u8 = 0x20;
const CMD_TRANSFER_PAD: u8 = 0x21;
/// Single-byte-transfer modifier bit on Transfer Info.
const CMD_SBT: u8 = 0x80;

// ----- SCSI status register codes (interrupt causes) ------------------------

pub const CSR_RESET: u8 = 0x00;
pub const CSR_RESET_AF: u8 = 0x01;
pub const CSR_SELECT: u8 = 0x11;
pub const CSR_SEL_XFER_DONE: u8 = 0x16;
pub const CSR_XFER_DONE: u8 = 0x18;
/// Paused: message-in byte received, ACK held until Negate ACK.
pub const CSR_MSGIN: u8 = 0x20;
pub const CSR_ABORTED: u8 = 0x28;
pub const CSR_TIMEOUT: u8 = 0x42;
/// Terminated: unexpected phase change; the low nibble carries the new
/// bus phase (0x4B = status phase while data was expected).
pub const CSR_UNEXP: u8 = 0x48;
pub const CSR_DISC: u8 = 0x85;
/// Unsolicited target REQ while no transfer command is in progress; the
/// low bits carry the requested bus phase.
pub const CSR_SRV_REQ: u8 = 0x88;

// SCSI bus information-transfer phase codes (MSG/C-D/I-O), as encoded in
// the low bits of phase-qualified CSR codes.
const PHS_DATA_OUT: u8 = 0x00;
const PHS_DATA_IN: u8 = 0x01;
const PHS_COMMAND: u8 = 0x02;
const PHS_STATUS: u8 = 0x03;
const PHS_MESS_OUT: u8 = 0x06;
const PHS_MESS_IN: u8 = 0x07;

// ----- SCSI status bytes ------------------------------------------------------

pub const GOOD: u8 = 0x00;
pub const CHECK_CONDITION: u8 = 0x02;

// Sense keys / additional sense codes.
const SK_HARDWARE_ERROR: u8 = 0x04;
const SK_ILLEGAL_REQUEST: u8 = 0x05;
const ASC_INVALID_OPCODE: u8 = 0x20;
const ASC_LBA_OUT_OF_RANGE: u8 = 0x21;
const ASC_INVALID_FIELD_IN_CDB: u8 = 0x24;
const ASC_LUN_NOT_SUPPORTED: u8 = 0x25;
const SENSE_LEN: usize = 18;

/// Emulated delay (chip-bus colour clocks) between issuing a command and
/// its completion interrupt, so drivers see busy-then-interrupt rather
/// than an instant flip mid-instruction.
const CMD_DELAY_CCK: u32 = 64;

fn be16(b: &[u8], off: usize) -> u32 {
    (u32::from(b[off]) << 8) | u32::from(b[off + 1])
}

fn be24(b: &[u8], off: usize) -> u32 {
    (u32::from(b[off]) << 16) | (u32::from(b[off + 1]) << 8) | u32::from(b[off + 2])
}

fn be32(b: &[u8], off: usize) -> u32 {
    (be24(b, off) << 8) | u32::from(b[off + 3])
}

/// CDB length from the opcode's command group.
fn cdb_len(opcode: u8) -> usize {
    match opcode >> 5 {
        0 => 6,
        1 | 2 => 10,
        4 => 16,
        5 => 12,
        _ => 6,
    }
}

// ----- disk target -----------------------------------------------------------

/// What a CDB asks the target to do next on the bus.
pub enum ScsiExec {
    /// Data-in phase: the target returns these bytes, then status.
    DataIn(Vec<u8>),
    /// Data-out phase: the target expects this many bytes, then status
    /// (resolved by [`ScsiDisk::complete_out`]).
    DataOut(usize),
    /// Straight to status.
    NoData,
}

/// A SCSI-2 direct-access (disk) target backed by a hard-drive image.
#[derive(serde::Serialize, serde::Deserialize)]
pub struct ScsiDisk {
    pub disk: HardDriveImage,
    sense: [u8; SENSE_LEN],
}

impl ScsiDisk {
    /// Open a SCSI unit. `unit` is the SCSI ID, which picks the DHn device
    /// name a synthesized RDB advertises (so a bare hardfile on unit 0
    /// boots as DH0 exactly as it would on the IDE bus). `volume_name`
    /// labels a directory mounted as an in-memory FFS volume.
    pub fn open(path: &Path, unit: usize, volume_name: Option<&str>) -> anyhow::Result<Self> {
        let disk = HardDriveImage::open(
            path,
            &format!("DH{unit}"),
            "scsi",
            "COPPERLINE SCSI DISK",
            volume_name,
        )?;
        Ok(Self {
            disk,
            sense: [0u8; SENSE_LEN],
        })
    }

    fn set_sense(&mut self, key: u8, asc: u8) {
        self.sense = [0u8; SENSE_LEN];
        self.sense[0] = 0x70; // current error, fixed format
        self.sense[2] = key;
        self.sense[7] = 10; // additional sense length
        self.sense[12] = asc;
    }

    fn clear_sense(&mut self) {
        self.sense = [0u8; SENSE_LEN];
    }

    fn check(&mut self, key: u8, asc: u8) -> (ScsiExec, u8) {
        self.set_sense(key, asc);
        (ScsiExec::NoData, CHECK_CONDITION)
    }

    fn geometry(&self) -> (u32, u32, u32) {
        let heads = crate::harddrive::RDB_HEADS;
        let spt = crate::harddrive::RDB_SPT;
        let cyls = (self.disk.total_sectors() / u64::from(heads * spt)).max(1) as u32;
        (cyls, heads, spt)
    }

    fn inquiry_data(lun: u8) -> Vec<u8> {
        let mut d = vec![0u8; 36];
        // Peripheral qualifier 011b + type 1Fh for an unsupported LUN.
        d[0] = if lun == 0 { 0x00 } else { 0x7F };
        d[2] = 0x02; // SCSI-2
        d[3] = 0x02; // response data format: SCSI-2
        d[4] = 31; // additional length
        d[8..16].copy_from_slice(b"COPPERLN");
        d[16..32].copy_from_slice(b"SCSI DISK       ");
        d[32..36].copy_from_slice(b"1.0 ");
        d
    }

    fn mode_pages(&self, page: u8) -> Option<Vec<u8>> {
        let (cyls, heads, spt) = self.geometry();
        let page3 = || {
            let mut p = vec![0u8; 24];
            p[0] = 0x03;
            p[1] = 22;
            p[3] = 1; // tracks per zone
            p[10] = (spt >> 8) as u8; // sectors per track
            p[11] = spt as u8;
            p[12] = (SECTOR_SIZE >> 8) as u8; // data bytes per physical sector
            p[13] = SECTOR_SIZE as u8;
            p[15] = 1; // interleave
            p[20] = 0x40; // hard sectored
            p
        };
        let page4 = || {
            let mut p = vec![0u8; 24];
            p[0] = 0x04;
            p[1] = 22;
            p[2] = (cyls >> 16) as u8;
            p[3] = (cyls >> 8) as u8;
            p[4] = cyls as u8;
            p[5] = heads as u8;
            p[20] = 0x1C; // 7200 rpm
            p[21] = 0x20;
            p
        };
        match page {
            0x03 => Some(page3()),
            0x04 => Some(page4()),
            0x3F => {
                let mut all = page3();
                all.extend_from_slice(&page4());
                Some(all)
            }
            _ => None,
        }
    }

    /// Parse and execute a CDB up to (but not including) any data-out
    /// payload. Returns the bus data phase and the status byte (for
    /// data-out commands the status is resolved by `complete_out`).
    pub fn execute(&mut self, cdb: &[u8], lun: u8) -> (ScsiExec, u8) {
        let Some(&op) = cdb.first() else {
            return self.check(SK_ILLEGAL_REQUEST, ASC_INVALID_FIELD_IN_CDB);
        };
        // The manual (Select + Transfer Info) path sizes the command
        // phase from the driver's TC register, not from the opcode's
        // real CDB length, so a short/malformed TC can hand us a `cdb`
        // slice shorter than what this opcode's fields need. Reject it
        // as a malformed CDB instead of indexing out of bounds below.
        if cdb.len() < cdb_len(op) {
            return self.check(SK_ILLEGAL_REQUEST, ASC_INVALID_FIELD_IN_CDB);
        }
        // REQUEST SENSE must report (then clear) the previous command's
        // sense data; every other command starts with it cleared.
        if op != 0x03 {
            self.clear_sense();
        }
        if lun != 0 {
            return match op {
                // INQUIRY for an unsupported LUN reports qualifier 011b.
                0x12 => {
                    let alloc = usize::from(cdb[4]);
                    let data = Self::inquiry_data(lun);
                    (ScsiExec::DataIn(data[..alloc.min(36)].to_vec()), GOOD)
                }
                _ => self.check(SK_ILLEGAL_REQUEST, ASC_LUN_NOT_SUPPORTED),
            };
        }
        let total = self.disk.total_sectors();
        match op {
            // TEST UNIT READY / REZERO UNIT
            0x00 | 0x01 => (ScsiExec::NoData, GOOD),
            // REQUEST SENSE
            0x03 => {
                let alloc = match cdb[4] {
                    0 => 4, // SCSI-1: zero means four bytes
                    n => usize::from(n),
                };
                let data = self.sense[..alloc.min(SENSE_LEN)].to_vec();
                self.clear_sense();
                (ScsiExec::DataIn(data), GOOD)
            }
            // FORMAT UNIT: the image is always "formatted". With FmtData a
            // short parameter list follows.
            0x04 => {
                if cdb[1] & 0x10 != 0 {
                    (ScsiExec::DataOut(4), GOOD)
                } else {
                    (ScsiExec::NoData, GOOD)
                }
            }
            // READ(6) / WRITE(6)
            0x08 | 0x0A => {
                let lba = u64::from(be24(cdb, 1) & 0x1F_FFFF);
                let count = match cdb[4] {
                    0 => 256u64,
                    n => u64::from(n),
                };
                self.rw_command(op == 0x08, lba, count, total)
            }
            // SEEK(6) / SEEK(10)
            0x0B | 0x2B => (ScsiExec::NoData, GOOD),
            // INQUIRY
            0x12 => {
                if cdb[1] & 0x01 != 0 {
                    // EVPD: only the supported-pages page.
                    if cdb[2] == 0x00 {
                        let data = [0u8, 0, 0, 1, 0];
                        let alloc = usize::from(cdb[4]);
                        return (ScsiExec::DataIn(data[..alloc.min(5)].to_vec()), GOOD);
                    }
                    return self.check(SK_ILLEGAL_REQUEST, ASC_INVALID_FIELD_IN_CDB);
                }
                let alloc = usize::from(cdb[4]);
                let data = Self::inquiry_data(0);
                (ScsiExec::DataIn(data[..alloc.min(36)].to_vec()), GOOD)
            }
            // MODE SELECT(6): accept and ignore the parameter list.
            0x15 => (ScsiExec::DataOut(usize::from(cdb[4])), GOOD),
            // RESERVE / RELEASE
            0x16 | 0x17 => (ScsiExec::NoData, GOOD),
            // MODE SENSE(6)
            0x1A => {
                let dbd = cdb[1] & 0x08 != 0;
                let page = cdb[2] & 0x3F;
                let Some(pages) = self.mode_pages(page) else {
                    return self.check(SK_ILLEGAL_REQUEST, ASC_INVALID_FIELD_IN_CDB);
                };
                let mut data = Vec::new();
                data.extend_from_slice(&[0, 0, 0, if dbd { 0 } else { 8 }]);
                if !dbd {
                    // Block descriptor: density 0, all blocks, 512-byte blocks.
                    let blocks = total.min(0x00FF_FFFF) as u32;
                    data.push(0);
                    data.extend_from_slice(&blocks.to_be_bytes()[1..]);
                    data.push(0);
                    data.extend_from_slice(&(SECTOR_SIZE as u32).to_be_bytes()[1..]);
                }
                data.extend_from_slice(&pages);
                data[0] = (data.len() - 1) as u8;
                let alloc = usize::from(cdb[4]);
                data.truncate(alloc);
                (ScsiExec::DataIn(data), GOOD)
            }
            // START STOP UNIT / SEND DIAGNOSTIC / PREVENT ALLOW REMOVAL
            0x1B | 0x1D | 0x1E => (ScsiExec::NoData, GOOD),
            // READ CAPACITY(10)
            0x25 => {
                let last = total.saturating_sub(1).min(u64::from(u32::MAX)) as u32;
                let mut data = Vec::with_capacity(8);
                data.extend_from_slice(&last.to_be_bytes());
                data.extend_from_slice(&(SECTOR_SIZE as u32).to_be_bytes());
                (ScsiExec::DataIn(data), GOOD)
            }
            // READ(10) / WRITE(10)
            0x28 | 0x2A => {
                let lba = u64::from(be32(cdb, 2));
                let count = u64::from(be16(cdb, 7));
                if count == 0 {
                    return (ScsiExec::NoData, GOOD);
                }
                self.rw_command(op == 0x28, lba, count, total)
            }
            // VERIFY(10)
            0x2F => {
                let lba = u64::from(be32(cdb, 2));
                let count = u64::from(be16(cdb, 7));
                if lba + count > total {
                    return self.check(SK_ILLEGAL_REQUEST, ASC_LBA_OUT_OF_RANGE);
                }
                (ScsiExec::NoData, GOOD)
            }
            // SYNCHRONIZE CACHE(10)
            0x35 => (ScsiExec::NoData, GOOD),
            // READ DEFECT DATA(10): no defects.
            0x37 => (ScsiExec::DataIn(vec![0, cdb[2] & 0x1F, 0, 0]), GOOD),
            _ => {
                log::debug!("scsi: unsupported opcode {op:#04X}");
                self.check(SK_ILLEGAL_REQUEST, ASC_INVALID_OPCODE)
            }
        }
    }

    fn rw_command(&mut self, read: bool, lba: u64, count: u64, total: u64) -> (ScsiExec, u8) {
        if lba + count > total {
            return self.check(SK_ILLEGAL_REQUEST, ASC_LBA_OUT_OF_RANGE);
        }
        let bytes = (count as usize) * SECTOR_SIZE;
        if !read {
            return (ScsiExec::DataOut(bytes), GOOD);
        }
        let mut data = vec![0u8; bytes];
        for i in 0..count {
            if let Err(e) = self
                .disk
                .read_sector(lba + i, &mut data[(i as usize) * SECTOR_SIZE..])
            {
                log::warn!(
                    "scsi {}: read lba {}: {e}",
                    self.disk.path().display(),
                    lba + i
                );
                return self.check(SK_HARDWARE_ERROR, 0x00);
            }
        }
        (ScsiExec::DataIn(data), GOOD)
    }

    /// Complete a data-out command once the payload has arrived.
    pub fn complete_out(&mut self, cdb: &[u8], data: &[u8]) -> u8 {
        // A data-out phase is only ever reached after `execute` accepted the
        // command, which already rejects an empty or too-short CDB, so this
        // guard cannot fire in normal flow. It stays as belt-and-suspenders
        // against the raw indexing below: a malformed CDB commits nothing and
        // reports GOOD (a no-op), never a spurious write at a garbage LBA.
        let Some(&op) = cdb.first() else {
            return GOOD;
        };
        if cdb.len() < cdb_len(op) {
            return GOOD;
        }
        match op {
            0x0A | 0x2A => {
                let lba = if cdb[0] == 0x0A {
                    u64::from(be24(cdb, 1) & 0x1F_FFFF)
                } else {
                    u64::from(be32(cdb, 2))
                };
                for (i, sector) in data.chunks(SECTOR_SIZE).enumerate() {
                    if sector.len() < SECTOR_SIZE {
                        break;
                    }
                    if let Err(e) = self.disk.write_sector(lba + i as u64, sector) {
                        log::warn!(
                            "scsi {}: write lba {}: {e}",
                            self.disk.path().display(),
                            lba + i as u64
                        );
                        self.set_sense(SK_HARDWARE_ERROR, 0x00);
                        return CHECK_CONDITION;
                    }
                }
                self.disk.flush();
                GOOD
            }
            // MODE SELECT / FORMAT UNIT parameter lists: accepted, ignored.
            _ => GOOD,
        }
    }
}

// ----- WD33C93A --------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum DmaDir {
    /// Device to memory (SCSI data-in).
    In,
    /// Memory to device (SCSI data-out).
    Out,
}

/// Which command owns the active data phase, deciding the completion
/// interrupt it posts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
enum PhaseOwner {
    SelXfer,
    TransInfo,
}

/// Bus phase progression for the manual (Select + Transfer Info) path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
enum ManualPhase {
    MsgOut,
    Command,
    Data,
    Status,
    MsgIn,
    BusFree,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct Transaction {
    target_id: u8,
    cdb: Vec<u8>,
    /// Data-in bytes still owed to the host (drained from the front via
    /// `offset`), or the collected data-out payload.
    data: Vec<u8>,
    offset: usize,
    /// Data-out bytes the target expects.
    out_expected: usize,
    dir: Option<DmaDir>,
    /// Target status byte; for data-out commands resolved at commit time.
    status: u8,
}

#[derive(serde::Serialize, serde::Deserialize)]
pub struct Wd33c93 {
    regs: [u8; 0x20],
    sasr: u8,
    asr_int: bool,
    busy: bool,
    /// Completion interrupts waiting to be delivered: (CSR code, delay).
    /// The front entry's delay counts down only while no interrupt is
    /// currently asserted; reading the SCSI status register acknowledges
    /// the current one and arms the next.
    pending: VecDeque<(u8, u32)>,
    targets: [Option<ScsiDisk>; 8],
    xfer: Option<Transaction>,
    /// Active data phase routed to the DMAC (the board pumps bytes).
    dma_dir: Option<DmaDir>,
    /// Active data phase routed through the data register (PIO).
    pio_dir: Option<DmaDir>,
    phase_owner: PhaseOwner,
    /// Manual-path bus phase after Select; None when bus free.
    manual: Option<ManualPhase>,
    /// Target id selected by a manual Select command.
    manual_target: Option<u8>,
    activity: bool,
}

impl Default for Wd33c93 {
    fn default() -> Self {
        Self::new()
    }
}

impl Wd33c93 {
    pub fn new() -> Self {
        Self {
            regs: [0u8; 0x20],
            sasr: 0,
            asr_int: false,
            busy: false,
            pending: VecDeque::new(),
            targets: Default::default(),
            xfer: None,
            dma_dir: None,
            pio_dir: None,
            phase_owner: PhaseOwner::SelXfer,
            manual: None,
            manual_target: None,
            activity: false,
        }
    }

    pub fn attach_target(&mut self, id: usize, disk: ScsiDisk) {
        self.targets[id.min(7)] = Some(disk);
    }

    pub fn target_present(&self, id: usize) -> bool {
        self.targets.get(id).is_some_and(Option::is_some)
    }

    /// Hardware reset (board /RST or system reset): clear chip state but
    /// keep the attached targets. No interrupt is posted.
    pub fn reset(&mut self) {
        self.regs = [0u8; 0x20];
        self.sasr = 0;
        self.asr_int = false;
        self.busy = false;
        self.pending.clear();
        self.xfer = None;
        self.dma_dir = None;
        self.pio_dir = None;
        self.manual = None;
        self.manual_target = None;
    }

    pub fn take_activity(&mut self) -> bool {
        std::mem::take(&mut self.activity)
    }

    /// The INTRQ output, as the DMAC's ISTR INTS bit sees it.
    pub fn int_asserted(&self) -> bool {
        self.asr_int
    }

    // ----- host port -------------------------------------------------------

    pub fn write_sasr(&mut self, v: u8) {
        self.sasr = v & 0x1F;
    }

    pub fn read_aux_status(&self) -> u8 {
        let mut v = 0u8;
        if self.asr_int {
            v |= ASR_INT;
        }
        if self.busy {
            v |= ASR_BSY | ASR_CIP;
        }
        if self.pio_dir.is_some() {
            v |= ASR_DBR;
        }
        v
    }

    fn incr_sasr(&mut self) {
        if matches!(self.sasr, WD_COMMAND | WD_DATA | 0x1F) {
            return;
        }
        self.sasr = (self.sasr + 1) & 0x1F;
    }

    pub fn read_data_port(&mut self) -> u8 {
        match self.sasr {
            WD_DATA => self.pio_read_byte(),
            WD_SCSI_STATUS => {
                let v = self.regs[usize::from(WD_SCSI_STATUS)];
                // Reading the status register acknowledges the interrupt;
                // the next queued cause (if any) re-raises after its delay.
                self.asr_int = false;
                self.incr_sasr();
                v
            }
            r => {
                let v = self.regs[usize::from(r)];
                self.incr_sasr();
                v
            }
        }
    }

    pub fn write_data_port(&mut self, v: u8) {
        match self.sasr {
            WD_COMMAND => self.execute_command(v),
            WD_DATA => self.pio_write_byte(v),
            r => {
                self.regs[usize::from(r)] = v;
                self.incr_sasr();
            }
        }
    }

    // ----- transfer count ----------------------------------------------------

    fn tc(&self) -> u32 {
        (u32::from(self.regs[usize::from(WD_TC_MSB)]) << 16)
            | (u32::from(self.regs[usize::from(WD_TC_MID)]) << 8)
            | u32::from(self.regs[usize::from(WD_TC_LSB)])
    }

    fn set_tc(&mut self, v: u32) {
        self.regs[usize::from(WD_TC_MSB)] = (v >> 16) as u8;
        self.regs[usize::from(WD_TC_MID)] = (v >> 8) as u8;
        self.regs[usize::from(WD_TC_LSB)] = v as u8;
    }

    fn dec_tc(&mut self) {
        let tc = self.tc();
        if tc > 0 {
            self.set_tc(tc - 1);
        }
    }

    /// Whether the control register routes data phases to the DMAC.
    fn dma_mode(&self) -> bool {
        self.regs[usize::from(WD_CONTROL)] >> 5 != 0
    }

    // ----- interrupt queue ----------------------------------------------------

    fn post(&mut self, csr: u8, delay: u32) {
        self.pending.push_back((csr, delay));
    }

    /// Advance emulated time: deliver the next queued interrupt cause once
    /// its delay has elapsed (and the previous one has been acknowledged).
    pub fn tick(&mut self, cck: u32) {
        if self.asr_int {
            return;
        }
        let Some(front) = self.pending.front_mut() else {
            return;
        };
        front.1 = front.1.saturating_sub(cck);
        if front.1 == 0 {
            let (csr, _) = self.pending.pop_front().unwrap();
            self.regs[usize::from(WD_SCSI_STATUS)] = csr;
            self.asr_int = true;
            self.busy = false;
        }
    }

    fn timeout_delay(&self) -> u32 {
        // The timeout period register counts units derived from the input
        // clock; at the A2091's clock one unit is in the 0.1-0.2 ms range.
        // 512 colour clocks per unit lands in the same ballpark and keeps
        // absent-device probes brisk.
        u32::from(self.regs[usize::from(WD_TIMEOUT)].max(1)) * 512
    }

    // ----- command execution ----------------------------------------------------

    fn execute_command(&mut self, cmd: u8) {
        self.regs[usize::from(WD_COMMAND)] = cmd;
        self.activity = true;
        match cmd & 0x7F {
            CMD_RESET => {
                let advanced = self.regs[usize::from(WD_OWN_ID)] & 0x08 != 0;
                let own_id = self.regs[usize::from(WD_OWN_ID)];
                self.reset();
                self.regs[usize::from(WD_OWN_ID)] = own_id;
                self.busy = true;
                self.post(
                    if advanced { CSR_RESET_AF } else { CSR_RESET },
                    CMD_DELAY_CCK,
                );
            }
            CMD_ABORT => {
                self.xfer = None;
                self.dma_dir = None;
                self.pio_dir = None;
                self.manual = None;
                self.busy = true;
                self.post(CSR_ABORTED, CMD_DELAY_CCK);
            }
            CMD_ASSERT_ATN => {
                // Immediate command: no interrupt.
            }
            CMD_NEGATE_ACK => {
                // Immediate, except after a message-in byte: releasing ACK
                // lets the target complete the command and go bus-free.
                if self.manual == Some(ManualPhase::BusFree) {
                    self.manual = None;
                    self.manual_target = None;
                    self.xfer = None;
                    self.regs[usize::from(WD_COMMAND_PHASE)] = 0x60;
                    self.busy = true;
                    self.post(CSR_DISC, CMD_DELAY_CCK);
                }
            }
            CMD_DISCONNECT => {
                self.manual = None;
                self.manual_target = None;
                self.xfer = None;
                self.busy = true;
                self.post(CSR_DISC, CMD_DELAY_CCK);
            }
            CMD_SELECT_ATN | CMD_SELECT => {
                let id = usize::from(self.regs[usize::from(WD_DESTINATION_ID)] & 7);
                self.busy = true;
                if self.target_present(id) {
                    let atn = cmd & 0x7F == CMD_SELECT_ATN;
                    self.manual_target = Some(id as u8);
                    self.manual = Some(if atn {
                        ManualPhase::MsgOut
                    } else {
                        ManualPhase::Command
                    });
                    self.regs[usize::from(WD_COMMAND_PHASE)] = if atn { 0x10 } else { 0x20 };
                    self.post(CSR_SELECT, CMD_DELAY_CCK);
                    // The target then asserts REQ for the first phase, which
                    // surfaces as a service-required interrupt once the
                    // selection status has been acknowledged.
                    self.post(
                        CSR_SRV_REQ | if atn { PHS_MESS_OUT } else { PHS_COMMAND },
                        CMD_DELAY_CCK,
                    );
                } else {
                    self.post(CSR_TIMEOUT, self.timeout_delay());
                }
            }
            CMD_SELECT_ATN_XFER | CMD_SELECT_XFER => self.sel_xfer(),
            CMD_TRANSFER_INFO | CMD_TRANSFER_PAD => self.trans_info(cmd & CMD_SBT != 0),
            other => {
                log::warn!("wd33c93: unimplemented command {other:#04X}");
            }
        }
    }

    // ----- select-and-transfer ----------------------------------------------------

    fn sel_xfer(&mut self) {
        self.busy = true;
        // Resume after a phase-mismatch pause: command phase register 0x45+
        // means selection/command already happened; skip to the data or
        // status handling for the transaction in flight.
        if self.regs[usize::from(WD_COMMAND_PHASE)] >= 0x45 && self.xfer.is_some() {
            let has_data = {
                let t = self.xfer.as_ref().unwrap();
                match t.dir {
                    Some(DmaDir::In) => t.offset < t.data.len() && self.tc() > 0,
                    Some(DmaDir::Out) => t.data.len() < t.out_expected && self.tc() > 0,
                    None => false,
                }
            };
            self.phase_owner = PhaseOwner::SelXfer;
            if has_data {
                self.start_data_phase();
            } else {
                self.finish_to_status();
            }
            return;
        }

        let id = usize::from(self.regs[usize::from(WD_DESTINATION_ID)] & 7);
        if !self.target_present(id) {
            self.xfer = None;
            self.regs[usize::from(WD_COMMAND_PHASE)] = 0x00;
            self.post(CSR_TIMEOUT, self.timeout_delay());
            return;
        }
        let lun = self.regs[usize::from(WD_TARGET_LUN)] & 7;
        let len = cdb_len(self.regs[usize::from(WD_CDB_1)]);
        let cdb: Vec<u8> = self.regs[usize::from(WD_CDB_1)..usize::from(WD_CDB_1) + len].to_vec();
        let (exec, status) = self.targets[id].as_mut().unwrap().execute(&cdb, lun);
        let mut txn = Transaction {
            target_id: id as u8,
            cdb,
            data: Vec::new(),
            offset: 0,
            out_expected: 0,
            dir: None,
            status,
        };
        match exec {
            ScsiExec::DataIn(data) if !data.is_empty() => {
                txn.dir = Some(DmaDir::In);
                txn.data = data;
            }
            ScsiExec::DataOut(expected) if expected > 0 => {
                txn.dir = Some(DmaDir::Out);
                txn.out_expected = expected;
            }
            _ => {}
        }
        self.phase_owner = PhaseOwner::SelXfer;
        let wants_data = txn.dir.is_some();
        self.xfer = Some(txn);
        if wants_data && self.tc() > 0 {
            self.start_data_phase();
        } else {
            if wants_data {
                log::warn!(
                    "wd33c93: select-and-transfer with zero transfer count but a data \
                     phase pending; skipping the data phase"
                );
            }
            self.finish_to_status();
        }
    }

    fn start_data_phase(&mut self) {
        let dir = self.xfer.as_ref().and_then(|t| t.dir);
        let Some(dir) = dir else {
            self.finish_to_status();
            return;
        };
        self.regs[usize::from(WD_COMMAND_PHASE)] = 0x45;
        if self.dma_mode() {
            self.dma_dir = Some(dir);
        } else {
            self.pio_dir = Some(dir);
        }
    }

    /// End of the data phase (or no data phase at all): receive the status
    /// byte and command-complete message, post the completion interrupt
    /// pair, and go bus-free.
    fn finish_to_status(&mut self) {
        self.dma_dir = None;
        self.pio_dir = None;
        let Some(txn) = self.xfer.take() else {
            return;
        };
        let status = self.commit_status(txn);
        self.regs[usize::from(WD_TARGET_LUN)] = status;
        self.regs[usize::from(WD_COMMAND_PHASE)] = 0x60;
        self.post(CSR_SEL_XFER_DONE, CMD_DELAY_CCK);
        // The target then disconnects, which interrupts again once the
        // completion status has been acknowledged.
        self.post(CSR_DISC, CMD_DELAY_CCK);
    }

    /// Resolve the transaction's final status byte, committing a data-out
    /// payload to the target.
    fn commit_status(&mut self, txn: Transaction) -> u8 {
        if txn.dir == Some(DmaDir::Out) {
            let id = usize::from(txn.target_id);
            if txn.data.len() < txn.out_expected {
                log::warn!(
                    "wd33c93: data-out phase ended {} bytes short of the target's \
                     expectation; padding with zeroes",
                    txn.out_expected - txn.data.len()
                );
            }
            let mut data = txn.data;
            data.resize(txn.out_expected, 0);
            if let Some(target) = self.targets[id].as_mut() {
                return target.complete_out(&txn.cdb, &data);
            }
        }
        txn.status
    }

    /// The data phase stalled because the target changed phase while the
    /// transfer count was still non-zero (short data-in): terminate with
    /// the unexpected-phase status so the driver can resume into status.
    fn pause_unexpected_status(&mut self) {
        self.dma_dir = None;
        self.pio_dir = None;
        self.busy = true;
        self.regs[usize::from(WD_COMMAND_PHASE)] = 0x46;
        self.post(CSR_UNEXP | PHS_STATUS, CMD_DELAY_CCK);
    }

    // ----- DMA handshake (board-side port) ----------------------------------------

    pub fn dma_request(&self) -> Option<DmaDir> {
        self.dma_dir
    }

    /// Bytes the chip is still willing to hand over / take in the current
    /// DMA data phase.
    pub fn dma_remaining(&self) -> usize {
        let tc = self.tc() as usize;
        match (self.dma_dir, self.xfer.as_ref()) {
            (Some(DmaDir::In), Some(t)) => tc.min(t.data.len() - t.offset),
            (Some(DmaDir::Out), Some(t)) => tc.min(t.out_expected - t.data.len()),
            _ => 0,
        }
    }

    pub fn dma_in_byte(&mut self) -> u8 {
        let Some(txn) = self.xfer.as_mut() else {
            return 0;
        };
        let b = txn.data.get(txn.offset).copied().unwrap_or(0);
        txn.offset += 1;
        self.dec_tc();
        self.activity = true;
        self.check_data_phase_end();
        b
    }

    pub fn dma_out_byte(&mut self, b: u8) {
        let Some(txn) = self.xfer.as_mut() else {
            return;
        };
        if txn.data.len() < txn.out_expected {
            txn.data.push(b);
        }
        self.dec_tc();
        self.activity = true;
        self.check_data_phase_end();
    }

    fn check_data_phase_end(&mut self) {
        let (dir, target_done) = {
            let Some(txn) = self.xfer.as_ref() else {
                return;
            };
            let done = match txn.dir {
                Some(DmaDir::In) => txn.offset >= txn.data.len(),
                Some(DmaDir::Out) => txn.data.len() >= txn.out_expected,
                None => true,
            };
            (txn.dir, done)
        };
        let tc_done = self.tc() == 0;
        if !target_done && !tc_done {
            return;
        }
        match self.phase_owner {
            PhaseOwner::SelXfer => {
                if target_done && !tc_done {
                    // Short data: the target moved to status early.
                    self.pause_unexpected_status();
                } else {
                    if tc_done && !target_done {
                        log::warn!(
                            "wd33c93: transfer count exhausted before the target's data; \
                             dropping the residue"
                        );
                        if let Some(t) = self.xfer.as_mut() {
                            t.offset = t.data.len();
                        }
                    }
                    self.finish_to_status();
                }
            }
            PhaseOwner::TransInfo => {
                self.dma_dir = None;
                self.pio_dir = None;
                self.busy = true;
                if dir == Some(DmaDir::Out) {
                    self.manual_out_complete();
                } else {
                    self.manual_in_complete();
                }
            }
        }
    }

    // ----- PIO data register --------------------------------------------------------

    fn pio_read_byte(&mut self) -> u8 {
        if self.pio_dir != Some(DmaDir::In) {
            return self.regs[usize::from(WD_DATA)];
        }
        self.dma_in_byte()
    }

    fn pio_write_byte(&mut self, b: u8) {
        if self.pio_dir != Some(DmaDir::Out) {
            self.regs[usize::from(WD_DATA)] = b;
            return;
        }
        self.dma_out_byte(b);
    }

    // ----- manual transfer path ---------------------------------------------------

    /// Transfer Info: move the current phase's bytes by hand. The manual
    /// path drives one phase at a time: message-out (identify), command
    /// (CDB), data, status, message-in.
    fn trans_info(&mut self, single_byte: bool) {
        let Some(phase) = self.manual else {
            // Transfer Info without a manual selection: step the sel-xfer
            // transaction's status/message by hand (some drivers do this
            // after a phase-mismatch pause).
            if self.xfer.is_some() {
                self.phase_owner = PhaseOwner::SelXfer;
                self.finish_to_status();
            }
            return;
        };
        self.busy = true;
        let count = if single_byte {
            1
        } else {
            self.tc().max(1) as usize
        };
        match phase {
            ManualPhase::MsgOut => {
                // Expect the identify message byte through the data port.
                self.phase_owner = PhaseOwner::TransInfo;
                self.xfer = Some(Transaction {
                    target_id: self.manual_target.unwrap_or(0),
                    cdb: Vec::new(),
                    data: Vec::new(),
                    offset: 0,
                    out_expected: 1,
                    dir: Some(DmaDir::Out),
                    status: GOOD,
                });
                self.arm_manual_data(DmaDir::Out);
            }
            ManualPhase::Command => {
                self.phase_owner = PhaseOwner::TransInfo;
                self.xfer = Some(Transaction {
                    target_id: self.manual_target.unwrap_or(0),
                    cdb: Vec::new(),
                    data: Vec::new(),
                    offset: 0,
                    out_expected: count,
                    dir: Some(DmaDir::Out),
                    status: GOOD,
                });
                self.arm_manual_data(DmaDir::Out);
            }
            ManualPhase::Data => {
                // Data phase already staged by the command-phase commit.
                self.phase_owner = PhaseOwner::TransInfo;
                self.start_data_phase();
            }
            ManualPhase::Status => {
                let status = self.xfer.as_ref().map_or(GOOD, |t| t.status);
                self.phase_owner = PhaseOwner::TransInfo;
                self.xfer = Some(Transaction {
                    target_id: self.manual_target.unwrap_or(0),
                    cdb: Vec::new(),
                    data: vec![status],
                    offset: 0,
                    out_expected: 0,
                    dir: Some(DmaDir::In),
                    status,
                });
                self.arm_manual_data(DmaDir::In);
            }
            ManualPhase::MsgIn => {
                self.phase_owner = PhaseOwner::TransInfo;
                self.xfer = Some(Transaction {
                    target_id: self.manual_target.unwrap_or(0),
                    cdb: Vec::new(),
                    data: vec![0x00], // command complete
                    offset: 0,
                    out_expected: 0,
                    dir: Some(DmaDir::In),
                    status: GOOD,
                });
                self.arm_manual_data(DmaDir::In);
            }
            ManualPhase::BusFree => {
                self.post(CSR_DISC, CMD_DELAY_CCK);
            }
        }
    }

    fn arm_manual_data(&mut self, dir: DmaDir) {
        // Manual transfers honour the DMA-mode bit like the combo command.
        if self.dma_mode() {
            self.dma_dir = Some(dir);
        } else {
            self.pio_dir = Some(dir);
        }
    }

    /// Called when a manual data-out segment completes: route the collected
    /// bytes to where the phase says they belong.
    fn manual_out_complete(&mut self) {
        let Some(phase) = self.manual else {
            return;
        };
        let Some(txn) = self.xfer.take() else {
            return;
        };
        match phase {
            ManualPhase::MsgOut => {
                // Identify message: 0x80 | LUN.
                let lun = txn.data.first().copied().unwrap_or(0x80) & 7;
                self.regs[usize::from(WD_TARGET_LUN)] =
                    (self.regs[usize::from(WD_TARGET_LUN)] & !7) | lun;
                self.manual = Some(ManualPhase::Command);
                self.regs[usize::from(WD_COMMAND_PHASE)] = 0x20;
                self.post(CSR_XFER_DONE | PHS_COMMAND, CMD_DELAY_CCK);
            }
            ManualPhase::Command => {
                let id = usize::from(txn.target_id);
                let lun = self.regs[usize::from(WD_TARGET_LUN)] & 7;
                if let Some(target) = self.targets[id].as_mut() {
                    let (exec, status) = target.execute(&txn.data, lun);
                    let mut next = Transaction {
                        target_id: txn.target_id,
                        cdb: txn.data.clone(),
                        data: Vec::new(),
                        offset: 0,
                        out_expected: 0,
                        dir: None,
                        status,
                    };
                    let csr = match exec {
                        ScsiExec::DataIn(data) if !data.is_empty() => {
                            next.dir = Some(DmaDir::In);
                            next.data = data;
                            self.manual = Some(ManualPhase::Data);
                            CSR_XFER_DONE | PHS_DATA_IN
                        }
                        ScsiExec::DataOut(expected) if expected > 0 => {
                            next.dir = Some(DmaDir::Out);
                            next.out_expected = expected;
                            self.manual = Some(ManualPhase::Data);
                            CSR_XFER_DONE | PHS_DATA_OUT
                        }
                        _ => {
                            self.manual = Some(ManualPhase::Status);
                            CSR_XFER_DONE | PHS_STATUS
                        }
                    };
                    self.regs[usize::from(WD_COMMAND_PHASE)] =
                        if self.manual == Some(ManualPhase::Data) {
                            0x45
                        } else {
                            0x46
                        };
                    self.xfer = Some(next);
                    self.post(csr, CMD_DELAY_CCK);
                }
            }
            ManualPhase::Data => {
                // Data-out payload complete: commit and stage the status.
                let status = self.commit_status(txn);
                self.xfer = Some(Transaction {
                    target_id: 0,
                    cdb: Vec::new(),
                    data: Vec::new(),
                    offset: 0,
                    out_expected: 0,
                    dir: None,
                    status,
                });
                self.manual = Some(ManualPhase::Status);
                self.regs[usize::from(WD_COMMAND_PHASE)] = 0x46;
                self.post(CSR_XFER_DONE | PHS_STATUS, CMD_DELAY_CCK);
            }
            _ => {}
        }
    }

    /// A manual data-in segment completed: advance the bus phase and tell
    /// the driver which phase the target requests next.
    fn manual_in_complete(&mut self) {
        match self.manual {
            Some(ManualPhase::Data) => {
                self.manual = Some(ManualPhase::Status);
                self.regs[usize::from(WD_COMMAND_PHASE)] = 0x46;
                self.post(CSR_XFER_DONE | PHS_STATUS, CMD_DELAY_CCK);
            }
            Some(ManualPhase::Status) => {
                self.manual = Some(ManualPhase::MsgIn);
                self.regs[usize::from(WD_COMMAND_PHASE)] = 0x50;
                self.post(CSR_XFER_DONE | PHS_MESS_IN, CMD_DELAY_CCK);
            }
            Some(ManualPhase::MsgIn) => {
                // The message byte sits in the chip with ACK asserted; the
                // driver inspects it and issues Negate ACK to finish.
                self.manual = Some(ManualPhase::BusFree);
                self.post(CSR_MSGIN, CMD_DELAY_CCK);
            }
            _ => {
                self.post(CSR_XFER_DONE, CMD_DELAY_CCK);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn rand_suffix() -> u64 {
        use std::sync::atomic::{AtomicU64, Ordering};
        use std::time::{SystemTime, UNIX_EPOCH};
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .subsec_nanos() as u64;
        (nanos << 16) | NEXT.fetch_add(1, Ordering::Relaxed)
    }

    fn temp_image(sectors: u64) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "copperline-scsi-test-{}-{}.hdf",
            std::process::id(),
            rand_suffix()
        ));
        let data = vec![0u8; (sectors * SECTOR_SIZE as u64) as usize];
        std::fs::write(&path, data).unwrap();
        path
    }

    fn chip_with_disk(sectors: u64) -> (Wd33c93, PathBuf) {
        let path = temp_image(sectors);
        let mut wd = Wd33c93::new();
        wd.attach_target(0, ScsiDisk::open(&path, 0, None).unwrap());
        (wd, path)
    }

    fn wr_reg(wd: &mut Wd33c93, reg: u8, val: u8) {
        wd.write_sasr(reg);
        wd.write_data_port(val);
    }

    fn rd_reg(wd: &mut Wd33c93, reg: u8) -> u8 {
        wd.write_sasr(reg);
        wd.read_data_port()
    }

    fn run_until_int(wd: &mut Wd33c93) {
        for _ in 0..10_000 {
            wd.tick(16);
            if wd.int_asserted() {
                return;
            }
        }
        panic!("no interrupt delivered");
    }

    fn set_cdb(wd: &mut Wd33c93, cdb: &[u8]) {
        wd.write_sasr(WD_CDB_1);
        for b in cdb {
            // The register pointer auto-increments through the CDB file.
            wd.write_data_port(*b);
        }
    }

    fn set_tc(wd: &mut Wd33c93, tc: u32) {
        wr_reg(wd, WD_TC_MSB, (tc >> 16) as u8);
        wd.write_data_port((tc >> 8) as u8);
        wd.write_data_port(tc as u8);
    }

    /// Issue a PIO-mode Select-with-ATN-and-Transfer and pump the data
    /// register; returns the data-in bytes and the final CSR code.
    fn sel_xfer_pio(
        wd: &mut Wd33c93,
        id: u8,
        cdb: &[u8],
        tc: u32,
        data_out: Option<&[u8]>,
    ) -> (Vec<u8>, u8) {
        wr_reg(wd, WD_CONTROL, 0x00); // PIO
        wr_reg(wd, WD_DESTINATION_ID, id);
        wr_reg(wd, WD_TARGET_LUN, 0);
        set_cdb(wd, cdb);
        set_tc(wd, tc);
        wr_reg(wd, WD_COMMAND, 0x08);
        let mut data_in = Vec::new();
        if let Some(out) = data_out {
            wd.write_sasr(WD_DATA);
            for b in out {
                assert!(wd.read_aux_status() & ASR_DBR != 0, "DBR for data-out");
                wd.write_data_port(*b);
            }
        } else {
            wd.write_sasr(WD_DATA);
            while wd.read_aux_status() & ASR_DBR != 0 {
                data_in.push(wd.read_data_port());
            }
        }
        run_until_int(wd);
        let csr = rd_reg(wd, WD_SCSI_STATUS);
        (data_in, csr)
    }

    #[test]
    fn reset_command_posts_advanced_reset_status() {
        let (mut wd, path) = chip_with_disk(64);
        wr_reg(&mut wd, WD_OWN_ID, 0x08 | 7); // EAF + own id 7
        wr_reg(&mut wd, WD_COMMAND, 0x00);
        assert!(wd.read_aux_status() & ASR_BSY != 0);
        run_until_int(&mut wd);
        assert_eq!(rd_reg(&mut wd, WD_SCSI_STATUS), CSR_RESET_AF);
        assert!(wd.read_aux_status() & ASR_INT == 0);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn selection_of_absent_target_times_out() {
        let (mut wd, path) = chip_with_disk(64);
        wr_reg(&mut wd, WD_TIMEOUT, 2);
        wr_reg(&mut wd, WD_DESTINATION_ID, 3); // nothing at ID 3
        set_cdb(&mut wd, &[0, 0, 0, 0, 0, 0]);
        set_tc(&mut wd, 0);
        wr_reg(&mut wd, WD_COMMAND, 0x08);
        run_until_int(&mut wd);
        assert_eq!(rd_reg(&mut wd, WD_SCSI_STATUS), CSR_TIMEOUT);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn inquiry_via_pio_completes_with_identity() {
        let (mut wd, path) = chip_with_disk(64);
        let (data, csr) = sel_xfer_pio(&mut wd, 0, &[0x12, 0, 0, 0, 36, 0], 36, None);
        assert_eq!(csr, CSR_SEL_XFER_DONE);
        assert_eq!(data.len(), 36);
        assert_eq!(data[0], 0x00); // direct access device
        assert_eq!(&data[8..16], b"COPPERLN");
        // The target's status byte lands in the Target LUN register.
        assert_eq!(rd_reg(&mut wd, WD_TARGET_LUN), GOOD);
        assert_eq!(rd_reg(&mut wd, WD_COMMAND_PHASE), 0x60);
        // Acknowledging the completion surfaces the disconnect interrupt.
        run_until_int(&mut wd);
        assert_eq!(rd_reg(&mut wd, WD_SCSI_STATUS), CSR_DISC);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn short_mode_sense_pauses_then_resumes_into_status() {
        let (mut wd, path) = chip_with_disk(2048);
        // Ask for 254 bytes; the target's mode data is much shorter, so
        // the target changes to status phase with the transfer count
        // still non-zero.
        let (data, csr) = sel_xfer_pio(&mut wd, 0, &[0x1A, 0, 0x03, 0, 254, 0], 254, None);
        assert_eq!(csr, CSR_UNEXP | PHS_STATUS);
        assert!(!data.is_empty() && data.len() < 254);
        assert_eq!(rd_reg(&mut wd, WD_COMMAND_PHASE), 0x46);
        // Mode data header + block descriptor + page 3.
        assert_eq!(data[3], 8);
        assert_eq!(data[12], 0x03);
        // The driver resumes the combination command from the status phase.
        wr_reg(&mut wd, WD_COMMAND, 0x08);
        run_until_int(&mut wd);
        assert_eq!(rd_reg(&mut wd, WD_SCSI_STATUS), CSR_SEL_XFER_DONE);
        assert_eq!(rd_reg(&mut wd, WD_TARGET_LUN), GOOD);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn write10_then_read10_roundtrips_through_the_image() {
        let (mut wd, path) = chip_with_disk(64);
        let mut payload = vec![0u8; SECTOR_SIZE];
        for (i, b) in payload.iter_mut().enumerate() {
            *b = (i % 251) as u8;
        }
        let (_, csr) = sel_xfer_pio(
            &mut wd,
            0,
            &[0x2A, 0, 0, 0, 0, 5, 0, 0, 1, 0],
            SECTOR_SIZE as u32,
            Some(&payload),
        );
        assert_eq!(csr, CSR_SEL_XFER_DONE);
        assert_eq!(rd_reg(&mut wd, WD_TARGET_LUN), GOOD);
        rd_reg(&mut wd, WD_SCSI_STATUS); // ack the disconnect
        run_until_int(&mut wd);
        rd_reg(&mut wd, WD_SCSI_STATUS);

        let (data, csr) = sel_xfer_pio(
            &mut wd,
            0,
            &[0x28, 0, 0, 0, 0, 5, 0, 0, 1, 0],
            SECTOR_SIZE as u32,
            None,
        );
        assert_eq!(csr, CSR_SEL_XFER_DONE);
        assert_eq!(data, payload);
        // And the bytes really live in the file at LBA 5.
        let on_disk = std::fs::read(&path).unwrap();
        assert_eq!(&on_disk[5 * SECTOR_SIZE..6 * SECTOR_SIZE], &payload[..]);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn illegal_opcode_checks_condition_and_request_sense_reports_it() {
        let (mut wd, path) = chip_with_disk(64);
        let (_, csr) = sel_xfer_pio(&mut wd, 0, &[0x5E, 0, 0, 0, 0, 0], 0, None);
        assert_eq!(csr, CSR_SEL_XFER_DONE);
        assert_eq!(rd_reg(&mut wd, WD_TARGET_LUN), CHECK_CONDITION);
        rd_reg(&mut wd, WD_SCSI_STATUS); // ack disconnect
        run_until_int(&mut wd);
        rd_reg(&mut wd, WD_SCSI_STATUS);

        let (sense, csr) = sel_xfer_pio(&mut wd, 0, &[0x03, 0, 0, 0, 18, 0], 18, None);
        assert_eq!(csr, CSR_SEL_XFER_DONE);
        assert_eq!(sense[0], 0x70);
        assert_eq!(sense[2], SK_ILLEGAL_REQUEST);
        assert_eq!(sense[12], ASC_INVALID_OPCODE);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn execute_rejects_cdb_shorter_than_its_opcode_group_instead_of_panicking() {
        // The manual (Select + Transfer Info) path sizes the command phase
        // from the driver's TC register, not the opcode's real CDB length,
        // so `execute()` can be handed a truncated `cdb` slice. It must
        // reject that as a malformed CDB rather than indexing past the end.
        let path = temp_image(64);
        let mut disk = ScsiDisk::open(&path, 0, None).unwrap();

        // Empty CDB.
        let (exec, status) = disk.execute(&[], 0);
        assert!(matches!(exec, ScsiExec::NoData));
        assert_eq!(status, CHECK_CONDITION);

        // WRITE(10) (0x2A) needs a 10-byte CDB; a short one must not panic
        // when the opcode handler indexes cdb[7] for the transfer length.
        let (exec, status) = disk.execute(&[0x2A, 0, 0, 0], 0);
        assert!(matches!(exec, ScsiExec::NoData));
        assert_eq!(status, CHECK_CONDITION);

        // A correctly-sized CDB for the same opcode still works.
        let (exec, status) = disk.execute(&[0x2A, 0, 0, 0, 0, 0, 0, 0, 1, 0], 0);
        assert!(matches!(exec, ScsiExec::DataOut(n) if n == SECTOR_SIZE));
        assert_eq!(status, GOOD);

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn complete_out_rejects_short_cdb_instead_of_panicking() {
        let path = temp_image(64);
        let mut disk = ScsiDisk::open(&path, 0, None).unwrap();
        assert_eq!(disk.complete_out(&[], &[0u8; SECTOR_SIZE]), GOOD);
        assert_eq!(disk.complete_out(&[0x2A, 0, 0], &[0u8; SECTOR_SIZE]), GOOD);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn read_capacity_reports_last_lba_and_block_size() {
        let (mut wd, path) = chip_with_disk(2048);
        let (data, csr) = sel_xfer_pio(&mut wd, 0, &[0x25, 0, 0, 0, 0, 0, 0, 0, 0, 0], 8, None);
        assert_eq!(csr, CSR_SEL_XFER_DONE);
        assert_eq!(u32::from_be_bytes(data[0..4].try_into().unwrap()), 2047);
        assert_eq!(u32::from_be_bytes(data[4..8].try_into().unwrap()), 512);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn nonzero_lun_inquiry_reports_device_not_present() {
        let (mut wd, path) = chip_with_disk(64);
        wr_reg(&mut wd, WD_CONTROL, 0x00);
        wr_reg(&mut wd, WD_DESTINATION_ID, 0);
        wr_reg(&mut wd, WD_TARGET_LUN, 1);
        set_cdb(&mut wd, &[0x12, 0x20, 0, 0, 36, 0]);
        set_tc(&mut wd, 36);
        wr_reg(&mut wd, WD_COMMAND, 0x08);
        wd.write_sasr(WD_DATA);
        let mut data = Vec::new();
        while wd.read_aux_status() & ASR_DBR != 0 {
            data.push(wd.read_data_port());
        }
        run_until_int(&mut wd);
        assert_eq!(rd_reg(&mut wd, WD_SCSI_STATUS), CSR_SEL_XFER_DONE);
        assert_eq!(data[0], 0x7F);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn test_unit_ready_has_no_data_phase() {
        let (mut wd, path) = chip_with_disk(64);
        let (data, csr) = sel_xfer_pio(&mut wd, 0, &[0x00, 0, 0, 0, 0, 0], 0, None);
        assert_eq!(csr, CSR_SEL_XFER_DONE);
        assert!(data.is_empty());
        assert_eq!(rd_reg(&mut wd, WD_TARGET_LUN), GOOD);
        std::fs::remove_file(&path).ok();
    }

    /// The phase-stepped transaction the A2091 7.0 boot ROM runs (verified
    /// against the real ROM): Select-with-ATN, identify message by
    /// single-byte Transfer Info, CDB by Transfer Info, PIO data, then a
    /// Select-and-Transfer reissued at command phase 0x46 to collect
    /// status and the command-complete message.
    #[test]
    fn boot_rom_style_manual_transaction_steps_each_phase() {
        let (mut wd, path) = chip_with_disk(64);
        wr_reg(&mut wd, WD_CONTROL, 0x00); // PIO
        wr_reg(&mut wd, WD_DESTINATION_ID, 0);

        // Select-with-ATN: selection completes, then the target's REQ for
        // the message-out phase surfaces as service-required.
        wr_reg(&mut wd, WD_COMMAND, 0x06);
        run_until_int(&mut wd);
        assert_eq!(rd_reg(&mut wd, WD_SCSI_STATUS), CSR_SELECT);
        run_until_int(&mut wd);
        assert_eq!(rd_reg(&mut wd, WD_SCSI_STATUS), CSR_SRV_REQ | PHS_MESS_OUT);

        // Identify message via single-byte Transfer Info.
        wr_reg(&mut wd, WD_COMMAND, 0xA0);
        assert!(wd.read_aux_status() & ASR_DBR != 0);
        wd.write_sasr(WD_DATA);
        wd.write_data_port(0xC0); // identify: disconnect allowed, LUN 0
        run_until_int(&mut wd);
        assert_eq!(rd_reg(&mut wd, WD_SCSI_STATUS), CSR_XFER_DONE | PHS_COMMAND);

        // CDB via Transfer Info: INQUIRY with a 254-byte allocation.
        wr_reg(&mut wd, WD_TC_MSB, 0);
        wd.write_data_port(0);
        wd.write_data_port(6);
        wr_reg(&mut wd, WD_COMMAND, 0x20);
        wd.write_sasr(WD_DATA);
        for b in [0x12u8, 0, 0, 0, 0xFE, 0] {
            assert!(wd.read_aux_status() & ASR_DBR != 0);
            wd.write_data_port(b);
        }
        run_until_int(&mut wd);
        assert_eq!(rd_reg(&mut wd, WD_SCSI_STATUS), CSR_XFER_DONE | PHS_DATA_IN);

        // Data phase via Transfer Info, PIO: the target's 36 bytes arrive,
        // then the early move to status is reported.
        wr_reg(&mut wd, WD_TC_MSB, 0);
        wd.write_data_port(0);
        wd.write_data_port(0xFE);
        wr_reg(&mut wd, WD_COMMAND, 0x20);
        wd.write_sasr(WD_DATA);
        let mut data = Vec::new();
        while wd.read_aux_status() & ASR_DBR != 0 {
            data.push(wd.read_data_port());
        }
        assert_eq!(data.len(), 36);
        assert_eq!(&data[8..16], b"COPPERLN");
        run_until_int(&mut wd);
        assert_eq!(rd_reg(&mut wd, WD_SCSI_STATUS), CSR_XFER_DONE | PHS_STATUS);

        // The ROM finishes by reissuing Select-and-Transfer from phase
        // 0x46: status byte and command-complete message are absorbed.
        wr_reg(&mut wd, WD_COMMAND_PHASE, 0x46);
        wr_reg(&mut wd, WD_COMMAND, 0x09);
        run_until_int(&mut wd);
        assert_eq!(rd_reg(&mut wd, WD_SCSI_STATUS), CSR_SEL_XFER_DONE);
        assert_eq!(rd_reg(&mut wd, WD_TARGET_LUN), GOOD);
        run_until_int(&mut wd);
        assert_eq!(rd_reg(&mut wd, WD_SCSI_STATUS), CSR_DISC);
        std::fs::remove_file(&path).ok();
    }
}
