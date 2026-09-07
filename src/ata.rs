// SPDX-License-Identifier: GPL-3.0-or-later

//! ATA (IDE) task file and command engine, shared by the machines that have an
//! IDE port: the A600/A1200's Gayle ([`crate::gayle`]) and the A4000's
//! motherboard interface ([`crate::ide_a4000`]).
//!
//! Both are the same 16-bit ATA-1 cable with the same eight task-file registers
//! and the same control block; only the address decode and the gate array's own
//! registers differ, so the front-ends keep those and hand every register access
//! to [`AtaBus`].
//!
//! Transfers complete within the access that triggers them: a command reads or
//! writes its sectors immediately and BSY is never observable.

use crate::harddrive::{HardDriveImage, RDB_HEADS, RDB_SPT};
use std::path::Path;

pub use crate::harddrive::SECTOR_SIZE;
/// Maximum sectors per READ/WRITE MULTIPLE block we advertise in IDENTIFY
/// word 47 and accept from SET MULTIPLE.
pub const MAX_MULTIPLE: u8 = 16;

// ATA status bits. BSY is defined for completeness: transfers complete
// within the access in this model, so it is never observable.
#[allow(dead_code)]
pub(crate) const ST_BSY: u8 = 0x80;
pub(crate) const ST_DRDY: u8 = 0x40;
pub(crate) const ST_DSC: u8 = 0x10;
pub(crate) const ST_DRQ: u8 = 0x08;
pub(crate) const ST_ERR: u8 = 0x01;
// ATA error bits.
pub(crate) const ERR_ABRT: u8 = 0x04;
pub(crate) const ERR_IDNF: u8 = 0x10;
// Device control bits.
pub(crate) const CTL_NIEN: u8 = 0x02;
pub(crate) const CTL_SRST: u8 = 0x04;
// Device/head bits.
pub(crate) const DH_LBA: u8 = 0x40;
pub(crate) const DH_DRV: u8 = 0x10;

/// A register in the task file, or the control block's shared
/// alternate-status/device-control address.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdeReg {
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

/// The task-file register at `offset` bytes from the base of the file. Both
/// Amiga interfaces space the eight registers four bytes apart, and each
/// register occupies both 16-bit halves of its slot, so it answers at offsets
/// 4n and 4n+2 (the `& !0x02` folds the two halfword addresses together).
pub fn task_file_reg(offset: u32) -> Option<IdeReg> {
    Some(match offset & !0x02 {
        0x00 => IdeReg::Data,
        0x04 => IdeReg::ErrorFeature,
        0x08 => IdeReg::SectorCount,
        0x0C => IdeReg::SectorNumber,
        0x10 => IdeReg::CylLow,
        0x14 => IdeReg::CylHigh,
        0x18 => IdeReg::DriveHead,
        0x1C => IdeReg::StatusCommand,
        _ => return None,
    })
}

// Not `Copy`: the PACKET data-phase variants carry a `Vec<u8>` (the SCSI
// command engine's own data buffer, reused directly rather than copied
// sector-by-sector like the ATA disk path). Call sites that used to rely on
// `Transfer` being `Copy` now clone or `mem::replace` explicitly.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
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
    /// ATAPI PACKET (0xA0): the host is clocking the 12-byte command packet
    /// into the data port; `buf`/`buf_pos` hold it directly.
    PacketCmd,
    /// ATAPI PACKET data-in phase: `data` is the whole response from
    /// [`AtapiDrive::execute`], `pos` is how much of it has already been
    /// staged into `buf` (i.e. `data[pos..]` is not yet buffered), and
    /// `byte_limit` is the host's byte-count-limit from cyl_low/cyl_high at
    /// PACKET issue time, chunking `data` into DRQ blocks no larger than it.
    PacketDataIn {
        data: Vec<u8>,
        pos: usize,
        byte_limit: u16,
    },
    /// ATAPI PACKET data-out phase: `cdb` is replayed to
    /// [`AtapiDrive::complete_out`] once `received` reaches `expected`
    /// bytes, chunked the same way as the data-in phase.
    PacketDataOut {
        cdb: [u8; 12],
        expected: usize,
        byte_limit: u16,
        received: Vec<u8>,
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
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn materialize_host_disk(&mut self) -> anyhow::Result<()> {
        self.disk.materialize_host_disk()
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn pending_host_disk(&self) -> Option<(String, String, bool)> {
        self.disk.pending_host_disk()
    }

    /// Open an IDE unit; `unit` picks the DHn device name a synthesized RDB
    /// advertises, so callers with more than one master/slave pair on the
    /// bus (e.g. a multi-channel `[lide]` board) must pass a value that is
    /// unique across every drive on the bus, not just channel-relative
    /// master=0/slave=1. The path may be a raw HDF image file, or a host
    /// directory, which is built into an in-memory FFS or OFS volume at open
    /// time (`filesystem` picks which; directory mounts only -- ignored for
    /// every other path form). `volume_name` labels that volume (directory
    /// mounts only). `boot_pri` is the synthesized partition's `de_BootPri`.
    pub fn open(
        path: &Path,
        unit: usize,
        volume_name: Option<&str>,
        boot_pri: i8,
        filesystem: crate::diskimage::FileSystem,
    ) -> anyhow::Result<Self> {
        let disk = HardDriveImage::open(
            path,
            &format!("DH{unit}"),
            "ide",
            volume_name,
            boot_pri,
            filesystem,
        )?;
        Ok(Self::from_disk(disk))
    }

    pub(crate) fn from_disk(disk: HardDriveImage) -> Self {
        // The classic Amiga HDF geometry: 16 surfaces, 32 sectors per track
        // (what HDToolBox/RDB tooling defaults to), so the CHS the host
        // computes from an RDB's physical-drive block agrees with what the
        // drive decodes.
        let heads = RDB_HEADS as u8;
        let spt = RDB_SPT as u8;
        let cylinders =
            (disk.total_sectors() / (u64::from(heads) * u64::from(spt))).clamp(1, 65535) as u16;
        Self {
            disk,
            default_heads: heads,
            default_spt: spt,
            cylinders,
            heads,
            spt,
            multiple: 0,
        }
    }

    /// Open a real host disk as an IDE drive.
    ///
    /// The geometry comes from the disk's own capacity, exactly as it does
    /// for an image: the guest's driver reads the RDB the disk already
    /// carries, so nothing here invents a partition table over media that
    /// came out of a real Amiga. That is also why there is no unit number to
    /// pass -- it names the device a synthesized RDB advertises, and a real
    /// disk brings its own.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn open_host_disk(
        device: &str,
        fingerprint: Option<&str>,
        identity_confirmed: bool,
        writable: bool,
    ) -> anyhow::Result<Self> {
        let disk =
            HardDriveImage::open_device(device, fingerprint, identity_confirmed, "ide", writable)?;
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

    /// IDENTIFY DEVICE data. The Amiga IDE ports wire the drive's data bus
    /// byte-swapped relative to the 68000 (IDE D7-D0 land on CPU D15-D8), so
    /// the CPU reads every ATA word with its bytes exchanged. The ROM driver's
    /// scsi.device depends on this: it parses the stored block assuming PC byte
    /// order per word (its word helper at $FB788C and string helper at $FB7B22
    /// swap each pair back). Sector data is unaffected because the swap puts
    /// file bytes back in natural memory order. We therefore store each ATA
    /// word low-byte-first here, since the data port read returns
    /// `buf[2i] << 8 | buf[2i+1]`.
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

/// A device attached to one drive slot of an [`AtaBus`]: a plain ATA hard
/// disk, or an ATAPI CD-ROM behind the shared SCSI-2 command engine in
/// [`crate::scsi::cd`].
#[derive(serde::Serialize, serde::Deserialize)]
pub enum AtaDevice {
    Disk(IdeDrive),
    Atapi(AtapiDrive),
}

impl From<IdeDrive> for AtaDevice {
    fn from(drive: IdeDrive) -> Self {
        AtaDevice::Disk(drive)
    }
}

impl From<AtapiDrive> for AtaDevice {
    fn from(drive: AtapiDrive) -> Self {
        AtaDevice::Atapi(drive)
    }
}

/// An ATAPI CD-ROM drive on an ATA cable: a thin wrapper around the
/// bus-agnostic SCSI-2 CD-ROM command engine ([`crate::scsi::ScsiCdRom`],
/// the same one the SCSI host adapters use), reached through the PACKET
/// (0xA0) command instead of a WD33C93 select/transfer sequence.
#[derive(serde::Serialize, serde::Deserialize)]
pub struct AtapiDrive {
    cdrom: crate::scsi::ScsiCdRom,
}

impl AtapiDrive {
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        Ok(Self {
            cdrom: crate::scsi::ScsiCdRom::open(path)?,
        })
    }

    fn execute(&mut self, cdb: &[u8], lun: u8) -> (crate::scsi::ScsiExec, u8) {
        self.cdrom.execute(cdb, lun)
    }

    fn complete_out(&mut self, cdb: &[u8], data: &[u8]) -> u8 {
        self.cdrom.complete_out(cdb, data)
    }

    fn sense_key(&self) -> u8 {
        self.cdrom.sense_key()
    }

    /// IDENTIFY PACKET DEVICE data, in the same byte-swapped-word storage
    /// convention as [`IdeDrive::identify_block`] (see its doc comment): the
    /// Amiga IDE port's byte swap means each ATA word is stored low-byte-first
    /// here.
    fn identify_packet_block() -> Vec<u8> {
        let mut buf = vec![0u8; SECTOR_SIZE];
        let mut word = |idx: usize, val: u16| {
            buf[idx * 2] = (val & 0xFF) as u8;
            buf[idx * 2 + 1] = (val >> 8) as u8;
        };
        // Word 0: bit 15 set (ATAPI device), bits 13-12 = 00 (12-byte command
        // packet), bits 9-8 = 00 (DRQ asserted within 3ms of PACKET, typical
        // for a CD-ROM), bits 12-8 = 00101 (device type 5, CD-ROM). 0x85C0 is
        // the conventional ATAPI-4/5 CD-ROM signature word real drives report.
        word(0, 0x85C0);
        word(49, 0x0200); // LBA supported

        // Same swapped-pair string convention as `IdeDrive::identify_block`.
        let mut string = |start: usize, len_words: usize, text: &str| {
            let mut bytes = text.as_bytes().to_vec();
            bytes.resize(len_words * 2, b' ');
            for (i, pair) in bytes.chunks(2).enumerate() {
                buf[(start + i) * 2] = pair[1];
                buf[(start + i) * 2 + 1] = pair[0];
            }
        };
        string(10, 10, "CPRLN-CDROM0000");
        string(23, 4, "1.0 ");
        string(27, 20, "COPPERLINE ATAPI CDROM");
        buf
    }
}

/// One ATA cable: the master/slave pair, the task file they share, and the
/// command engine.
#[derive(serde::Serialize, serde::Deserialize)]
pub struct AtaBus {
    drives: [Option<AtaDevice>; 2],
    // Shared task file. On a real cable each device carries its own
    // register file, but host writes reach both devices at once (only the
    // DEV bit picks which one executes commands and answers reads), so a
    // single copy is observationally the same -- except for the cylinder
    // registers below.
    feature: u8,
    error: u8,
    sector_count: u8,
    sector_number: u8,
    /// Cylinder registers, one pair per device slot: a host write lands in
    /// both (task-file writes broadcast down the cable), a read comes from
    /// the selected slot, and a device's own updates (PACKET byte counts,
    /// task-file advance after a transfer) touch only its slot. Per-slot
    /// because these are where the post-reset device signature lives: a
    /// PACKET device's 0x14/0xEB has to sit beside the disk's zeros so
    /// device-select can reveal either without rewriting -- and clobbering
    /// -- a cylinder address the host just programmed.
    cyl_low: [u8; 2],
    cyl_high: [u8; 2],
    drive_head: u8,
    status: u8,
    devctl: u8,
    /// INTRQ, the drive's interrupt line: raised on command completion and on
    /// each DRQ block, dropped when the host reads the status register.
    intrq: bool,
    /// INTRQ went high since the front-end last looked. Gayle latches this in
    /// its own interrupt-change register.
    irq_edge: bool,

    buf: Vec<u8>,
    buf_pos: usize,
    transfer: Transfer,
    /// Byte-count-limit for the in-progress PACKET data phase, captured from
    /// cyl_low/cyl_high when the host issues 0xA0 (they are repurposed as an
    /// ordinary task-file register outside PACKET protocol).
    packet_byte_limit: u16,
    /// Set whenever the drive does real work (command issued or data port
    /// moved during a transfer); drained by the bus for the HDD LED.
    activity: bool,
}

impl Default for AtaBus {
    fn default() -> Self {
        Self::new()
    }
}

impl AtaBus {
    pub fn new() -> Self {
        Self {
            drives: [None, None],
            feature: 0,
            error: 0x01, // diagnostics passed
            sector_count: 0x01,
            sector_number: 0x01,
            cyl_low: [0; 2],
            cyl_high: [0; 2],
            drive_head: 0,
            status: ST_DRDY | ST_DSC,
            devctl: 0,
            intrq: false,
            irq_edge: false,
            buf: Vec::new(),
            buf_pos: 0,
            transfer: Transfer::None,
            packet_byte_limit: 0,
            activity: false,
        }
    }

    pub fn attach_drive(&mut self, slot: usize, drive: impl Into<AtaDevice>) {
        self.drives[slot.min(1)] = Some(drive.into());
    }

    /// Whether either drive slot is populated. A front-end whose cable
    /// connector is entirely unpopulated (as opposed to one drive present
    /// and the other slot empty) can use this to float every task-file
    /// register, not just status: [`Self::read_reg`] only special-cases
    /// status/alt-status for "no drive selected", so an unattached second
    /// physical channel needs this check to avoid presenting registers like
    /// device/head as a hard zero, which some drivers' probes read as "a
    /// device answered" rather than "floating bus".
    pub fn any_drive_attached(&self) -> bool {
        self.drives.iter().any(Option::is_some)
    }

    /// The first ATAPI CD-ROM drive on this cable, if either slot holds one;
    /// the runtime disc-swap target (`--insert-cd-after`, the status bar's CD
    /// buttons, the control protocol).
    pub fn first_atapi_ref(&self) -> Option<&crate::scsi::ScsiCdRom> {
        self.drives.iter().flatten().find_map(|d| match d {
            AtaDevice::Atapi(drive) => Some(&drive.cdrom),
            AtaDevice::Disk(_) => None,
        })
    }

    /// Mutable counterpart of [`Self::first_atapi_ref`].
    pub fn first_atapi_mut(&mut self) -> Option<&mut crate::scsi::ScsiCdRom> {
        self.drives.iter_mut().flatten().find_map(|d| match d {
            AtaDevice::Atapi(drive) => Some(&mut drive.cdrom),
            AtaDevice::Disk(_) => None,
        })
    }

    /// Advance every ATAPI drive on this cable (master and slave alike),
    /// not just [`Self::first_atapi_mut`]'s single disc-swap target -- a
    /// second CD-ROM's own pending-swap countdown or CD-DA playback needs
    /// its own tick just as much as the first's.
    pub fn tick_atapi(&mut self, cck: u32, cd_audio: &mut crate::chipset::paula::CdAudioRing) {
        for drive in self.drives.iter_mut().flatten() {
            if let AtaDevice::Atapi(drive) = drive {
                drive.cdrom.tick(cck, cd_audio);
            }
        }
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn pending_host_disks(&self, out: &mut Vec<(String, String, bool)>) {
        out.extend(self.drives.iter().flatten().filter_map(|d| match d {
            AtaDevice::Disk(disk) => disk.pending_host_disk(),
            // A CD image is never a "host disk" (a real disk lent by the
            // host); only ATA disks can be.
            AtaDevice::Atapi(_) => None,
        }));
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn materialize_host_disks(&mut self) -> anyhow::Result<()> {
        for drive in self.drives.iter_mut().flatten() {
            if let AtaDevice::Disk(disk) = drive {
                disk.materialize_host_disk()?;
            }
        }
        Ok(())
    }

    /// Let go of any real disk of the host's, and say how many went.
    ///
    /// A drive is powered by the machine, so a machine that is switched off
    /// does not hold one: the disk goes back to the host, where it can be
    /// unmounted, taken out, or given to the next machine this window builds.
    /// Image-backed drives stay exactly where they are -- a file is not held
    /// against anybody.
    pub fn release_host_disks(&mut self) -> usize {
        let mut released = 0;
        for (slot, drive) in self.drives.iter_mut().enumerate() {
            let is_host_disk = matches!(drive, Some(AtaDevice::Disk(d)) if d.disk.is_host_disk());
            if !is_host_disk {
                continue;
            }
            *drive = None;
            released += 1;
            log::info!(
                "ide: {} released; the machine is off and the host has the disk back",
                if slot == 0 { "master" } else { "slave" }
            );
        }
        released
    }

    /// System reset: clear the register file and any in-flight transfer but
    /// keep the mounted drives.
    pub fn reset(&mut self) {
        self.feature = 0;
        self.sector_count = 0x01;
        self.sector_number = 0x01;
        self.drive_head = 0;
        self.devctl = 0;
        // cyl_low/cyl_high are stamped by soft_reset(), below, since they
        // carry each device's post-reset signature.
        self.soft_reset();
    }

    /// Drain the activity latch set by command issue and data-port traffic.
    /// The bus polls this after each access to time the HDD LED.
    pub fn take_activity(&mut self) -> bool {
        std::mem::take(&mut self.activity)
    }

    /// The INTRQ line as the host sees it: masked by the control block's
    /// interrupt disable.
    pub fn irq_level(&self) -> bool {
        self.intrq && self.devctl & CTL_NIEN == 0
    }

    /// Drain the "INTRQ went high" edge, for a front-end that latches it.
    pub fn take_irq_edge(&mut self) -> bool {
        std::mem::take(&mut self.irq_edge)
    }

    fn selected(&self) -> usize {
        usize::from(self.drive_head & DH_DRV != 0)
    }

    fn drive(&mut self) -> Option<&mut IdeDrive> {
        match self.drives[self.selected()].as_mut() {
            Some(AtaDevice::Disk(drive)) => Some(drive),
            _ => None,
        }
    }

    fn atapi_drive(&mut self) -> Option<&mut AtapiDrive> {
        match self.drives[self.selected()].as_mut() {
            Some(AtaDevice::Atapi(drive)) => Some(drive),
            _ => None,
        }
    }

    fn selected_is_atapi(&self) -> bool {
        matches!(self.drives[self.selected()], Some(AtaDevice::Atapi(_)))
    }

    fn pair_present(&self) -> bool {
        self.drives[1 - self.selected().min(1)].is_some()
    }

    fn raise_irq(&mut self) {
        self.intrq = true;
        if self.devctl & CTL_NIEN == 0 {
            self.irq_edge = true;
        }
    }

    fn clear_irq(&mut self) {
        self.intrq = false;
    }

    // ----- register access ---------------------------------------------------

    pub fn read_reg(&mut self, reg: Option<IdeReg>, size: usize) -> u32 {
        // Selected device absent: the status register reads 0x01 (ERR set,
        // not ready) when the other device is present and 0xFF when the
        // cable is empty; every other task-file register reads zero, and a
        // status read drops a pending interrupt (the INTRQ line is shared).
        // This is how the ROM probe concludes a unit does not exist
        // instead of classifying it as a pre-ATA drive (matches WinUAE).
        if self.drives[self.selected()].is_none() {
            return match reg {
                Some(IdeReg::StatusCommand) | Some(IdeReg::AltStatusDevCtl) => {
                    self.clear_irq();
                    if self.pair_present() {
                        0x01
                    } else {
                        0xFF
                    }
                }
                _ => 0,
            };
        }
        match reg {
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
            Some(IdeReg::CylLow) => u32::from(self.cyl_low[self.selected()]),
            Some(IdeReg::CylHigh) => u32::from(self.cyl_high[self.selected()]),
            Some(IdeReg::DriveHead) => u32::from(self.drive_head),
            Some(IdeReg::StatusCommand) => {
                let v = self.status;
                self.clear_irq();
                u32::from(v)
            }
            Some(IdeReg::AltStatusDevCtl) => u32::from(self.status),
            None => 0,
        }
    }

    pub fn write_reg(&mut self, reg: Option<IdeReg>, size: usize, value: u32) {
        let byte = value as u8;
        match reg {
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
            // A cylinder write reaches both devices on the cable, so both
            // slots take the value; the per-slot split only shows once a
            // reset stamps each device's own signature.
            Some(IdeReg::CylLow) => self.cyl_low = [byte; 2],
            Some(IdeReg::CylHigh) => self.cyl_high = [byte; 2],
            // Selecting a device must not touch the cylinder registers: the
            // signature is placed only by a reset (and DEVICE RESET), never
            // by device-select. Drivers routinely program the cylinder
            // address before drive/head -- KS3.1 scsi.device does -- and a
            // select that rewrote them sent every read to cylinder 0.
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

    fn soft_reset(&mut self) {
        self.status = ST_DRDY | ST_DSC;
        self.error = 0x01;
        self.transfer = Transfer::None;
        self.buf.clear();
        self.buf_pos = 0;
        self.clear_irq();
        // A soft reset resets both devices on the cable, so both slots get
        // their signatures; a mixed disk+ATAPI bus probed by toggling
        // device-select afterwards (no further reset) sees each slot's own.
        self.stamp_signature(0);
        self.stamp_signature(1);
    }

    /// One slot's post-reset device signature (ATA/ATAPI-4 9.1): a PACKET
    /// device reports cyl_low/cyl_high = 0x14/0xEB (sector_count/
    /// sector_number are already 0x01/0x01 for either device type) so a
    /// host driver can probe for a PACKET device without issuing a command.
    /// A disk (or an absent slot) keeps the plain-ATA convention of zero.
    /// Stamped only by a reset -- and, for the one device it addresses, by
    /// DEVICE RESET -- never by device-select.
    fn stamp_signature(&mut self, slot: usize) {
        if matches!(self.drives[slot], Some(AtaDevice::Atapi(_))) {
            self.cyl_low[slot] = 0x14;
            self.cyl_high[slot] = 0xEB;
        } else {
            self.cyl_low[slot] = 0;
            self.cyl_high[slot] = 0;
        }
    }

    // ----- data port -------------------------------------------------------

    fn data_read_word(&mut self) -> u16 {
        if !matches!(
            self.transfer,
            Transfer::PioIn { .. } | Transfer::PacketDataIn { .. }
        ) || self.buf_pos + 1 >= self.buf.len()
        {
            return 0;
        }
        let word = (u16::from(self.buf[self.buf_pos]) << 8) | u16::from(self.buf[self.buf_pos + 1]);
        self.buf_pos += 2;
        self.activity = true;
        if self.buf_pos >= self.buf.len() {
            match self.transfer {
                Transfer::PioIn { .. } => self.pio_in_block_consumed(),
                Transfer::PacketDataIn { .. } => self.packet_in_block_consumed(),
                _ => unreachable!("gated by the matches! above"),
            }
        }
        word
    }

    fn data_write_word(&mut self, word: u16) {
        if !matches!(
            self.transfer,
            Transfer::PioOut { .. } | Transfer::PacketCmd | Transfer::PacketDataOut { .. }
        ) || self.buf_pos + 1 >= self.buf.len()
        {
            return;
        }
        self.buf[self.buf_pos] = (word >> 8) as u8;
        self.buf[self.buf_pos + 1] = (word & 0xFF) as u8;
        self.buf_pos += 2;
        self.activity = true;
        if self.buf_pos >= self.buf.len() {
            match self.transfer {
                Transfer::PioOut { .. } => self.pio_out_block_filled(),
                Transfer::PacketCmd => self.packet_command_received(),
                Transfer::PacketDataOut { .. } => self.packet_out_block_filled(),
                _ => unreachable!("gated by the matches! above"),
            }
        }
    }

    /// Pad an odd-length PACKET data-phase chunk to an even word count: the
    /// data port always moves whole words, so an odd byte count (a plausible
    /// REQUEST SENSE allocation length, for instance) needs one pad byte the
    /// host is expected to ignore -- the true byte count travels separately,
    /// in cyl_low/cyl_high (data-in) or is already known to the host
    /// (data-out).
    fn pad_to_even(mut v: Vec<u8>) -> Vec<u8> {
        if !v.len().is_multiple_of(2) {
            v.push(0);
        }
        v
    }

    fn pio_in_block_consumed(&mut self) {
        // `Transfer` is not `Copy` (the PACKET variants carry a `Vec<u8>`);
        // cloning is cheap here since this is only reached once `matches!`
        // in `data_read_word` has already confirmed `self.transfer` is the
        // (small, Copy-field-only) `PioIn` variant.
        let Transfer::PioIn { remaining, block } = self.transfer.clone() else {
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
            self.raise_irq();
        }
    }

    fn pio_out_block_filled(&mut self) {
        let Transfer::PioOut { remaining, block } = self.transfer.clone() else {
            return;
        };
        // Commit the buffered sectors at the current task-file position.
        if self.commit_write_buffer().is_err() {
            return;
        }
        if remaining == 0 {
            if let Some(drive) = self.drive() {
                if let Err(error) = drive.disk.flush() {
                    log::warn!("IDE flush: {error}");
                    self.command_error(ERR_ABRT);
                    return;
                }
            }
            self.status = ST_DRDY | ST_DSC;
            self.transfer = Transfer::None;
            self.raise_irq();
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
        self.raise_irq();
    }

    // ----- ATAPI PACKET protocol --------------------------------------------

    /// Set the task file's interrupt-reason overlay. There is no dedicated
    /// register in this model: PACKET protocol repurposes sector_count's low
    /// three bits (C/D, I/O, REL) for exactly this, the same register that
    /// otherwise only matters to plain ATA commands, which an ATAPI slot
    /// aborts before this is ever called.
    fn set_interrupt_reason(&mut self, cd: bool, io: bool) {
        self.sector_count = (self.sector_count & !0x07) | (u8::from(cd)) | (u8::from(io) << 1);
    }

    /// The error register's sense-key nibble a real ATAPI drive reports
    /// immediately on CHECK CONDITION, ahead of any REQUEST SENSE follow-up.
    fn atapi_sense_key_error(&mut self) -> u8 {
        let key = self.atapi_drive().map(|d| d.sense_key()).unwrap_or(0);
        (key & 0x0F) << 4
    }

    /// Abort an ATA-only command (READ SECTORS, IDENTIFY DEVICE, DEVICE
    /// RESET, ...) because the selected slot is ATAPI, not a disk. Beyond
    /// the usual `ERR_ABRT`, this also posts an ILLEGAL REQUEST sense entry
    /// on the ATAPI drive so a following REQUEST SENSE agrees with the error
    /// the driver just saw, instead of reporting "no sense". Scoped to this
    /// one case -- ordinary ATA-disk error paths (bad LBA, I/O error, ...)
    /// keep going through the plain `command_error`.
    fn atapi_type_mismatch_abort(&mut self) {
        if let Some(drive) = self.atapi_drive() {
            drive.cdrom.post_illegal_command_sense();
        }
        self.command_error(ERR_ABRT);
    }

    /// Finish a PACKET command that produced neither a data-in nor a
    /// data-out phase: go straight to the final status phase, mapping the
    /// SCSI status byte to the ATAPI error/status convention.
    fn packet_finish_no_data(&mut self, scsi_status: u8) {
        self.set_interrupt_reason(true, true); // C/D=1, I/O=1
        if scsi_status == crate::scsi::GOOD {
            self.error = 0;
            self.status = ST_DRDY | ST_DSC;
        } else {
            self.error = self.atapi_sense_key_error();
            self.status = ST_DRDY | ST_DSC | ST_ERR;
        }
        self.transfer = Transfer::None;
        self.raise_irq();
    }

    /// The 12-byte command packet has been fully clocked into `buf`: hand it
    /// to the ATAPI drive's SCSI command engine and set up whichever phase
    /// follows.
    fn packet_command_received(&mut self) {
        let cdb: [u8; 12] = self.buf[..12].try_into().unwrap();
        let byte_limit = self.packet_byte_limit;
        // A PACKET CDB's LBA (READ(10)/READ(12)/READ CD all put it at
        // cdb[2..6], big-endian) is not visible in `command()`'s own
        // COPPERLINE_DIAG_GAYLE trace -- that fires at 0xA0 issue, before
        // the CDB has been clocked in through the data port -- so trace it
        // separately here, once the CDB is actually in hand.
        if crate::envcfg::flag("COPPERLINE_DIAG_GAYLE") {
            log::info!(
                "ide packet cdb drv={} op={:#04X} lba={}",
                self.selected(),
                cdb[0],
                u32::from_be_bytes([cdb[2], cdb[3], cdb[4], cdb[5]])
            );
        }
        let Some(drive) = self.atapi_drive() else {
            // The selected drive vanished mid-command (should not happen in
            // practice); abort cleanly rather than panic.
            self.command_error(ERR_ABRT);
            return;
        };
        let (exec, scsi_status) = drive.execute(&cdb, 0);
        // A second trace, after execute() rather than before: the CDB-issue
        // trace above only proves the driver asked for something, not that
        // the command actually succeeded or returned real data -- a
        // regression that made execute() fail or return garbage would still
        // satisfy that one. For a data-in response, also note whether an
        // ISO9660 primary volume descriptor signature ("CD001") is present,
        // so a test reading a real disc's PVD can assert on content
        // actually round-tripping through the SCSI engine, not merely a
        // status byte.
        if crate::envcfg::flag("COPPERLINE_DIAG_GAYLE") {
            let outcome = match &exec {
                crate::scsi::ScsiExec::DataIn(data) => format!(
                    "data_in bytes={} status={scsi_status:#04X} pvd_signature={}",
                    data.len(),
                    data.windows(5).any(|w| w == b"CD001")
                ),
                crate::scsi::ScsiExec::DataOut(n) => {
                    format!("data_out bytes={n} status={scsi_status:#04X}")
                }
                crate::scsi::ScsiExec::NoData => format!("no_data status={scsi_status:#04X}"),
            };
            log::info!("ide packet result drv={} {outcome}", self.selected());
        }
        match exec {
            // A legitimate ATAPI idiom (INQUIRY/REQUEST SENSE/MODE SENSE
            // with an allocation length of 0): there is no DRQ phase to
            // enter at all, since `data_write_word`/`data_read_word` never
            // complete a zero-length `buf` -- going through
            // `Transfer::PacketDataIn` here would hang the bus forever with
            // DRQ asserted and no interrupt to follow.
            crate::scsi::ScsiExec::DataIn(data) if data.is_empty() => {
                self.packet_finish_no_data(scsi_status);
            }
            crate::scsi::ScsiExec::DataIn(data) => {
                self.set_interrupt_reason(false, true); // C/D=0, I/O=1
                let chunk = data.len().min(byte_limit as usize);
                self.buf = Self::pad_to_even(data[..chunk].to_vec());
                self.buf_pos = 0;
                self.set_packet_byte_count(chunk);
                self.transfer = Transfer::PacketDataIn {
                    data,
                    pos: chunk,
                    byte_limit,
                };
                self.status = ST_DRDY | ST_DSC | ST_DRQ;
                self.raise_irq();
            }
            // Same idiom as the empty DataIn case above, for a zero-length
            // data-out phase: no bytes to collect, so complete the command
            // immediately rather than entering `Transfer::PacketDataOut`
            // with an empty `buf` that `data_write_word` can never fill.
            crate::scsi::ScsiExec::DataOut(0) => {
                let status = self
                    .atapi_drive()
                    .map(|d| d.complete_out(&cdb, &[]))
                    .unwrap_or(crate::scsi::CHECK_CONDITION);
                self.packet_finish_no_data(status);
            }
            crate::scsi::ScsiExec::DataOut(expected) => {
                self.set_interrupt_reason(false, false); // C/D=0, I/O=0
                let chunk = expected.min(byte_limit as usize);
                self.buf = Self::pad_to_even(vec![0u8; chunk]);
                self.buf_pos = 0;
                self.set_packet_byte_count(chunk);
                self.transfer = Transfer::PacketDataOut {
                    cdb,
                    expected,
                    byte_limit,
                    received: Vec::new(),
                };
                // First DRQ block is ready without an interrupt, as for
                // WRITE SECTORS' first block.
                self.status = ST_DRDY | ST_DSC | ST_DRQ;
            }
            crate::scsi::ScsiExec::NoData => self.packet_finish_no_data(scsi_status),
        }
    }

    /// Report a PACKET data-phase chunk's byte count in the cylinder
    /// registers (ATAPI repurposes them for this during the data phase).
    /// The reporting device writes only its own register file, so the
    /// other slot's registers are untouched.
    fn set_packet_byte_count(&mut self, chunk: usize) {
        let slot = self.selected();
        self.cyl_low[slot] = (chunk & 0xFF) as u8;
        self.cyl_high[slot] = ((chunk >> 8) & 0xFF) as u8;
    }

    /// A PACKET data-in DRQ block has been fully read: stage the next chunk
    /// of `data`, or transition to the final status phase once all of it has
    /// been delivered.
    fn packet_in_block_consumed(&mut self) {
        let Transfer::PacketDataIn {
            data,
            pos,
            byte_limit,
        } = std::mem::replace(&mut self.transfer, Transfer::None)
        else {
            return;
        };
        if pos >= data.len() {
            self.packet_finish_no_data(crate::scsi::GOOD);
            return;
        }
        let chunk = (data.len() - pos).min(byte_limit as usize);
        self.buf = Self::pad_to_even(data[pos..pos + chunk].to_vec());
        self.buf_pos = 0;
        self.set_packet_byte_count(chunk);
        self.set_interrupt_reason(false, true); // C/D=0, I/O=1
        self.transfer = Transfer::PacketDataIn {
            data,
            pos: pos + chunk,
            byte_limit,
        };
        self.status = ST_DRDY | ST_DSC | ST_DRQ;
        self.raise_irq();
    }

    /// A PACKET data-out DRQ block has been fully written: fold it into the
    /// accumulated payload, then either stage the next chunk or, once
    /// `expected` bytes are in hand, complete the command.
    fn packet_out_block_filled(&mut self) {
        let Transfer::PacketDataOut {
            cdb,
            expected,
            byte_limit,
            mut received,
        } = std::mem::replace(&mut self.transfer, Transfer::None)
        else {
            return;
        };
        // Recompute this block's real (unpadded) length the same way it was
        // originally sized, so the pad byte `pad_to_even` may have appended
        // to `buf` (only ever the last byte) is not folded into the payload.
        let chunk = (expected - received.len()).min(byte_limit as usize);
        received.extend_from_slice(&self.buf[..chunk]);
        if received.len() >= expected {
            let scsi_status = self
                .atapi_drive()
                .map(|d| d.complete_out(&cdb, &received))
                .unwrap_or(crate::scsi::CHECK_CONDITION);
            self.packet_finish_no_data(scsi_status);
            return;
        }
        let next_chunk = (expected - received.len()).min(byte_limit as usize);
        self.buf = Self::pad_to_even(vec![0u8; next_chunk]);
        self.buf_pos = 0;
        self.set_packet_byte_count(next_chunk);
        self.transfer = Transfer::PacketDataOut {
            cdb,
            expected,
            byte_limit,
            received,
        };
        self.status = ST_DRDY | ST_DSC | ST_DRQ;
        // No interrupt for a mid-transfer block, matching WRITE SECTORS.
    }

    // ----- addressing -------------------------------------------------------

    /// Current LBA from the task file (LBA28 or CHS translation).
    fn current_lba(&mut self) -> Option<u64> {
        let lba_mode = self.drive_head & DH_LBA != 0;
        let head = u64::from(self.drive_head & 0x0F);
        let sector = u64::from(self.sector_number);
        let slot = self.selected();
        let cyl = (u64::from(self.cyl_high[slot]) << 8) | u64::from(self.cyl_low[slot]);
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
        // The device advancing its position writes only its own register
        // file, so the cylinder updates below stay in the selected slot.
        let slot = self.selected();
        if self.drive_head & DH_LBA != 0 {
            let lba = ((u32::from(self.drive_head & 0x0F) << 24)
                | (u32::from(self.cyl_high[slot]) << 16)
                | (u32::from(self.cyl_low[slot]) << 8)
                | u32::from(self.sector_number))
            .wrapping_add(1);
            self.sector_number = (lba & 0xFF) as u8;
            self.cyl_low[slot] = ((lba >> 8) & 0xFF) as u8;
            self.cyl_high[slot] = ((lba >> 16) & 0xFF) as u8;
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
        let cyl =
            ((u16::from(self.cyl_high[slot]) << 8) | u16::from(self.cyl_low[slot])).wrapping_add(1);
        self.cyl_low[slot] = (cyl & 0xFF) as u8;
        self.cyl_high[slot] = (cyl >> 8) as u8;
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
        self.raise_irq();
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
                self.cyl_high[self.selected()],
                self.cyl_low[self.selected()],
                self.sector_number,
                self.sector_count
            );
        }
        self.clear_irq();
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
            // IDENTIFY DEVICE: plain-ATA only. A real ATAPI drive aborts
            // this (it answers 0xA1 instead), which is how a host driver
            // tells "no drive" from "wrong IDENTIFY for a PACKET device".
            0xEC => {
                if self.selected_is_atapi() {
                    self.atapi_type_mismatch_abort();
                    return;
                }
                self.buf = self.drive().map(|d| d.identify_block()).unwrap_or_default();
                self.buf_pos = 0;
                self.transfer = Transfer::PioIn {
                    remaining: 0,
                    block: 1,
                };
                self.status = ST_DRDY | ST_DSC | ST_DRQ;
                self.raise_irq();
            }
            // IDENTIFY PACKET DEVICE: the ATAPI counterpart of 0xEC, and
            // likewise aborts against the wrong device type.
            0xA1 => {
                if !self.selected_is_atapi() {
                    self.command_error(ERR_ABRT);
                    return;
                }
                self.buf = AtapiDrive::identify_packet_block();
                self.buf_pos = 0;
                self.transfer = Transfer::PioIn {
                    remaining: 0,
                    block: 1,
                };
                self.status = ST_DRDY | ST_DSC | ST_DRQ;
                self.raise_irq();
            }
            // PACKET: hand a 12-byte SCSI command packet to the ATAPI
            // drive's command engine. cyl_low/cyl_high (an ordinary
            // register outside PACKET protocol) is repurposed by the host
            // to set the byte-count-limit for the data phase that follows.
            0xA0 => {
                if !self.selected_is_atapi() {
                    self.command_error(ERR_ABRT);
                    return;
                }
                let slot = self.selected();
                let raw_limit =
                    u16::from(self.cyl_low[slot]) | (u16::from(self.cyl_high[slot]) << 8);
                // 0 conventionally means "no limit"; rather than model an
                // unbounded transfer, clamp to a large default.
                self.packet_byte_limit = if raw_limit == 0 { 0xFFFE } else { raw_limit };
                self.buf = vec![0u8; 12];
                self.buf_pos = 0;
                self.transfer = Transfer::PacketCmd;
                self.status = ST_DRDY | ST_DSC | ST_DRQ;
                // Command-phase DRQ does not interrupt: the host polls
                // status for it, the same convention as WRITE SECTORS'
                // first block.
                self.set_interrupt_reason(true, false); // C/D=1, I/O=0
            }
            // DEVICE RESET: ATAPI-only per ATA-4 (a plain-ATA slot has no
            // such command and aborts it, like every other type-specific
            // command above). Narrower than a full soft reset -- it only
            // resets the selected drive's register file and re-asserts its
            // signature. Because this arm now only runs when the *selected*
            // drive is ATAPI, and only one drive can be mid-PACKET-transfer
            // at a time, buf/transfer here always belongs to the drive being
            // reset -- so this is also the driver's real recovery path out
            // of a wedged PACKET data phase (see the zero-length DataIn/
            // DataOut handling above for the case that no longer wedges).
            0x08 => {
                if !self.selected_is_atapi() {
                    // Not a mismatch in the "ATA command against ATAPI"
                    // sense (there's no ATAPI drive to post sense on) --
                    // just DEVICE RESET being invalid against a disk slot.
                    self.command_error(ERR_ABRT);
                    return;
                }
                self.error = 0x01;
                self.sector_count = 0x01;
                self.sector_number = 0x01;
                // DEVICE RESET addresses one device; only its slot's
                // signature is re-stamped.
                let slot = self.selected();
                self.stamp_signature(slot);
                self.transfer = Transfer::None;
                self.buf.clear();
                self.buf_pos = 0;
                self.status = ST_DRDY | ST_DSC;
                // Real DEVICE RESET does not raise a completion interrupt.
            }
            // READ SECTORS (with/without retry) and READ MULTIPLE.
            0x20 | 0x21 | 0xC4 => {
                if self.selected_is_atapi() {
                    self.atapi_type_mismatch_abort();
                    return;
                }
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
                    self.raise_irq();
                }
            }
            // WRITE SECTORS (with/without retry) and WRITE MULTIPLE.
            0x30 | 0x31 | 0xC5 => {
                if self.selected_is_atapi() {
                    self.atapi_type_mismatch_abort();
                    return;
                }
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
                if self.selected_is_atapi() {
                    self.atapi_type_mismatch_abort();
                    return;
                }
                let requested = self.sector_count;
                let ok =
                    requested <= MAX_MULTIPLE && (requested == 0 || requested.is_power_of_two());
                if let (true, Some(drive)) = (ok, self.drive()) {
                    drive.multiple = requested;
                    self.status = ST_DRDY | ST_DSC;
                    self.raise_irq();
                } else {
                    self.command_error(ERR_ABRT);
                }
            }
            // INITIALIZE DEVICE PARAMETERS: set current CHS translation.
            // A zero sector count is invalid and aborts, as on real drives.
            0x91 => {
                let heads = (self.drive_head & 0x0F) + 1;
                let spt = self.sector_count;
                if self.selected_is_atapi() {
                    self.atapi_type_mismatch_abort();
                    return;
                }
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
                self.raise_irq();
            }
            // RECALIBRATE
            0x10..=0x1F => {
                if self.selected_is_atapi() {
                    self.atapi_type_mismatch_abort();
                    return;
                }
                self.status = ST_DRDY | ST_DSC;
                self.raise_irq();
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
    use crate::scsi::GOOD;
    use std::path::PathBuf;

    fn rand_suffix() -> u64 {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(0);
        NEXT.fetch_add(1, Ordering::Relaxed)
    }

    /// A bare-ISO fixture of `sectors` 2048-byte data sectors, sector `n`
    /// filled with byte value `n` (truncated to u8) -- enough to tell
    /// sectors apart in a READ(10) test without needing a real filesystem.
    fn temp_cd_image(sectors: u32) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "copperline-ata-test-{}-{}.iso",
            std::process::id(),
            rand_suffix()
        ));
        let mut data = vec![0u8; sectors as usize * 2048];
        for s in 0..sectors {
            data[(s as usize) * 2048..(s as usize + 1) * 2048].fill(s as u8);
        }
        std::fs::write(&path, &data).unwrap();
        path
    }

    fn temp_disk_image(sectors: u64) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "copperline-ata-test-{}-{}.hdf",
            std::process::id(),
            rand_suffix()
        ));
        std::fs::write(&path, vec![0u8; (sectors * SECTOR_SIZE as u64) as usize]).unwrap();
        path
    }

    fn atapi_bus(sectors: u32) -> (AtaBus, PathBuf) {
        let path = temp_cd_image(sectors);
        let mut bus = AtaBus::new();
        bus.attach_drive(0, AtapiDrive::open(&path).unwrap());
        (bus, path)
    }

    /// Issue PACKET (0xA0) with `byte_limit` and clock `cdb` in through the
    /// data port, exactly as a real driver would.
    fn issue_packet(bus: &mut AtaBus, cdb: &[u8; 12], byte_limit: u16) {
        bus.write_reg(Some(IdeReg::CylLow), 2, u32::from(byte_limit & 0xFF));
        bus.write_reg(Some(IdeReg::CylHigh), 2, u32::from(byte_limit >> 8));
        bus.command(0xA0);
        assert_eq!(bus.status & ST_ERR, 0, "PACKET issue aborted");
        assert_ne!(bus.status & ST_DRQ, 0, "PACKET issue did not assert DRQ");
        for word in cdb.chunks(2) {
            let w = (u16::from(word[0]) << 8) | u16::from(word[1]);
            bus.data_write_word(w);
        }
    }

    /// Drain a PACKET data-in phase to the end (the final status phase),
    /// reassembling the real (unpadded) bytes from each DRQ block.
    fn packet_read_data(bus: &mut AtaBus) -> Vec<u8> {
        let mut out = Vec::new();
        while matches!(bus.transfer, Transfer::PacketDataIn { .. }) {
            let count = (bus.read_reg(Some(IdeReg::CylLow), 1)
                | (bus.read_reg(Some(IdeReg::CylHigh), 1) << 8)) as usize;
            let words = count.div_ceil(2);
            let mut block = Vec::with_capacity(words * 2);
            for _ in 0..words {
                let w = bus.data_read_word();
                block.push((w >> 8) as u8);
                block.push((w & 0xFF) as u8);
            }
            block.truncate(count);
            out.extend_from_slice(&block);
        }
        out
    }

    fn inquiry_cdb() -> [u8; 12] {
        let mut cdb = [0u8; 12];
        cdb[0] = 0x12; // INQUIRY
        cdb[4] = 36; // allocation length
        cdb
    }

    fn read10_cdb(lba: u32, count: u16) -> [u8; 12] {
        let mut cdb = [0u8; 12];
        cdb[0] = 0x28; // READ(10)
        cdb[2..6].copy_from_slice(&lba.to_be_bytes());
        cdb[7..9].copy_from_slice(&count.to_be_bytes());
        cdb
    }

    fn request_sense_cdb() -> [u8; 12] {
        let mut cdb = [0u8; 12];
        cdb[0] = 0x03; // REQUEST SENSE
        cdb[4] = 18; // allocation length
        cdb
    }

    #[test]
    fn identify_packet_device_answers_only_the_atapi_slot() {
        let (mut bus, _path) = atapi_bus(2);
        bus.command(0xA1);
        assert_eq!(bus.status & ST_ERR, 0);
        assert_ne!(bus.status & ST_DRQ, 0);
        // The data port returns each ATA word byte-swapped (the Amiga IDE
        // port's wiring, see `IdeDrive::identify_block`'s doc comment); undo
        // it to check the ATA-defined value.
        let word0 = bus.data_read_word().swap_bytes();
        assert_eq!(word0 & 0x8000, 0x8000, "bit 15 (ATAPI) must be set");
        assert_eq!(word0, 0x85C0);

        // The same command against a plain disk slot aborts.
        let disk_path = temp_disk_image(64);
        let mut disk_bus = AtaBus::new();
        disk_bus.attach_drive(
            0,
            IdeDrive::open(&disk_path, 0, None, 0, crate::diskimage::FileSystem::FFS).unwrap(),
        );
        disk_bus.command(0xA1);
        assert_ne!(disk_bus.status & ST_ERR, 0);
        assert_eq!(disk_bus.error, ERR_ABRT);
    }

    #[test]
    fn identify_device_aborts_against_an_atapi_slot() {
        let (mut bus, _path) = atapi_bus(2);
        bus.command(0xEC);
        assert_ne!(bus.status & ST_ERR, 0);
        assert_eq!(bus.error, ERR_ABRT);
    }

    #[test]
    fn atapi_signature_appears_only_on_the_atapi_slot() {
        let (mut bus, _path) = atapi_bus(2);
        bus.reset();
        assert_eq!(bus.sector_count, 0x01);
        assert_eq!(bus.sector_number, 0x01);
        assert_eq!(bus.read_reg(Some(IdeReg::CylLow), 1), 0x14);
        assert_eq!(bus.read_reg(Some(IdeReg::CylHigh), 1), 0xEB);

        let disk_path = temp_disk_image(64);
        let mut disk_bus = AtaBus::new();
        disk_bus.attach_drive(
            0,
            IdeDrive::open(&disk_path, 0, None, 0, crate::diskimage::FileSystem::FFS).unwrap(),
        );
        disk_bus.reset();
        assert_eq!(disk_bus.read_reg(Some(IdeReg::CylLow), 1), 0);
        assert_eq!(disk_bus.read_reg(Some(IdeReg::CylHigh), 1), 0);
    }

    /// A mixed bus reselected from the disk slot to the ATAPI slot (no reset
    /// in between) must show the ATAPI signature -- each device keeps its
    /// own post-reset registers, and device-select picks which pair the
    /// host reads back.
    #[test]
    fn each_slot_keeps_its_own_signature_across_reselection() {
        let disk_path = temp_disk_image(64);
        let cd_path = temp_cd_image(2);
        let mut bus = AtaBus::new();
        bus.attach_drive(
            0,
            IdeDrive::open(&disk_path, 0, None, 0, crate::diskimage::FileSystem::FFS).unwrap(),
        );
        bus.attach_drive(1, AtapiDrive::open(&cd_path).unwrap());
        bus.reset();

        // Select the disk slot: plain-ATA signature.
        bus.write_reg(Some(IdeReg::DriveHead), 1, 0);
        assert_eq!(bus.read_reg(Some(IdeReg::CylLow), 1), 0);
        assert_eq!(bus.read_reg(Some(IdeReg::CylHigh), 1), 0);

        // Select the ATAPI slot, with no intervening reset: its slot still
        // holds the signature stamped at reset.
        bus.write_reg(Some(IdeReg::DriveHead), 1, u32::from(DH_DRV));
        assert_eq!(bus.read_reg(Some(IdeReg::CylLow), 1), 0x14);
        assert_eq!(bus.read_reg(Some(IdeReg::CylHigh), 1), 0xEB);

        // And back to the disk slot: still zeros, not the ATAPI pair.
        bus.write_reg(Some(IdeReg::DriveHead), 1, 0);
        assert_eq!(bus.read_reg(Some(IdeReg::CylLow), 1), 0);
        assert_eq!(bus.read_reg(Some(IdeReg::CylHigh), 1), 0);
    }

    /// Host-written cylinder registers survive a drive-select write:
    /// task-file writes broadcast to both devices, and the signature is
    /// placed only by a reset, never by device-select. KS3.1's scsi.device
    /// programs the cylinder address before drive/head, so a select that
    /// rewrote the registers sent every disk read to cylinder 0 -- the
    /// synthesized RDB cylinder -- and the mounted partition came up
    /// "Not a DOS disk".
    #[test]
    fn host_written_cylinder_registers_survive_drive_select() {
        let disk_path = temp_disk_image(64);
        let cd_path = temp_cd_image(2);
        let mut bus = AtaBus::new();
        bus.attach_drive(
            0,
            IdeDrive::open(&disk_path, 0, None, 0, crate::diskimage::FileSystem::FFS).unwrap(),
        );
        bus.attach_drive(1, AtapiDrive::open(&cd_path).unwrap());
        bus.reset();

        bus.write_reg(Some(IdeReg::CylLow), 1, 0x34);
        bus.write_reg(Some(IdeReg::CylHigh), 1, 0x12);
        bus.write_reg(Some(IdeReg::DriveHead), 1, u32::from(DH_LBA));
        assert_eq!(bus.read_reg(Some(IdeReg::CylLow), 1), 0x34);
        assert_eq!(bus.read_reg(Some(IdeReg::CylHigh), 1), 0x12);

        // The write reached the other device too (broadcast), so selecting
        // it shows the same value -- the signature returns only at reset.
        bus.write_reg(Some(IdeReg::DriveHead), 1, u32::from(DH_DRV));
        assert_eq!(bus.read_reg(Some(IdeReg::CylLow), 1), 0x34);
        assert_eq!(bus.read_reg(Some(IdeReg::CylHigh), 1), 0x12);
        bus.reset();
        bus.write_reg(Some(IdeReg::DriveHead), 1, u32::from(DH_DRV));
        assert_eq!(bus.read_reg(Some(IdeReg::CylLow), 1), 0x14);
        assert_eq!(bus.read_reg(Some(IdeReg::CylHigh), 1), 0xEB);
    }

    #[test]
    fn packet_inquiry_matches_a_direct_scsi_call() {
        let (mut bus, path) = atapi_bus(2);
        let cdb = inquiry_cdb();
        issue_packet(&mut bus, &cdb, 0xFFFE);
        let got = packet_read_data(&mut bus);
        assert_eq!(bus.status & ST_DRQ, 0);
        assert_eq!(bus.status & ST_ERR, 0);

        let mut direct = crate::scsi::ScsiCdRom::open(&path).unwrap();
        let (exec, status) = direct.execute(&cdb, 0);
        assert_eq!(status, GOOD);
        let crate::scsi::ScsiExec::DataIn(expected) = exec else {
            panic!("INQUIRY did not return DataIn");
        };
        assert_eq!(got, expected);
    }

    #[test]
    fn packet_read10_chunks_across_byte_count_limit() {
        let (mut bus, path) = atapi_bus(8);
        let cdb = read10_cdb(0, 4);
        // A byte-count-limit smaller than the read forces several DRQ
        // blocks: 4 sectors * 2048 bytes = 8192, limited to 512-byte chunks.
        issue_packet(&mut bus, &cdb, 512);
        let got = packet_read_data(&mut bus);

        let mut direct = crate::scsi::ScsiCdRom::open(&path).unwrap();
        let (exec, status) = direct.execute(&cdb, 0);
        assert_eq!(status, GOOD);
        let crate::scsi::ScsiExec::DataIn(expected) = exec else {
            panic!("READ(10) did not return DataIn");
        };
        assert_eq!(got.len(), 4 * 2048);
        assert_eq!(got, expected);
    }

    #[test]
    fn packet_error_path_reports_sense_key_and_request_sense_agrees() {
        let (mut bus, _path) = atapi_bus(2);
        // An unsupported opcode: CHECK CONDITION / ILLEGAL REQUEST.
        let mut bad_cdb = [0u8; 12];
        bad_cdb[0] = 0xFF;
        issue_packet(&mut bus, &bad_cdb, 0xFFFE);
        assert_ne!(bus.status & ST_ERR, 0);
        assert_eq!(bus.status & ST_DRQ, 0);
        let sense_key = bus.error >> 4;
        assert_eq!(sense_key, crate::scsi::SK_ILLEGAL_REQUEST);

        // REQUEST SENSE afterwards reports (and matches) the same key.
        let sense_cdb = request_sense_cdb();
        issue_packet(&mut bus, &sense_cdb, 0xFFFE);
        let sense = packet_read_data(&mut bus);
        assert_eq!(sense[2] & 0x0F, sense_key);
    }

    #[test]
    fn mixed_bus_routes_commands_to_the_selected_slot_only() {
        let disk_path = temp_disk_image(64);
        let cd_path = temp_cd_image(2);
        let mut bus = AtaBus::new();
        bus.attach_drive(
            0,
            IdeDrive::open(&disk_path, 0, None, 0, crate::diskimage::FileSystem::FFS).unwrap(),
        );
        bus.attach_drive(1, AtapiDrive::open(&cd_path).unwrap());

        // Master (disk) selected: IDENTIFY DEVICE succeeds.
        bus.drive_head = 0;
        bus.command(0xEC);
        assert_eq!(bus.status & ST_ERR, 0);

        // Slave (ATAPI) selected: READ SECTORS aborts without touching the
        // disk slot, and the disk still answers correctly afterwards.
        bus.drive_head = DH_DRV;
        bus.sector_count = 1;
        bus.command(0x20);
        assert_ne!(bus.status & ST_ERR, 0);
        assert_eq!(bus.error, ERR_ABRT);

        bus.drive_head = 0;
        bus.command(0xEC);
        assert_eq!(bus.status & ST_ERR, 0, "disk slot must be unaffected");
    }

    /// A PACKET command with a legitimately empty response (INQUIRY with
    /// allocation length 0, a normal ATAPI idiom) must complete straight
    /// away rather than entering a DRQ data-in phase with an empty buffer --
    /// `data_read_word` can never clock an empty buffer to completion, so
    /// that path hangs the bus forever with DRQ asserted and no interrupt.
    #[test]
    fn packet_zero_length_data_in_completes_without_hanging() {
        let (mut bus, _path) = atapi_bus(2);
        let mut cdb = inquiry_cdb();
        cdb[4] = 0; // allocation length 0: a legitimate zero-byte response
        issue_packet(&mut bus, &cdb, 0xFFFE);
        assert_eq!(
            bus.transfer,
            Transfer::None,
            "must not enter a DRQ data-in phase with nothing to transfer"
        );
        assert_eq!(bus.status & ST_DRQ, 0, "DRQ must not stay asserted");
        assert_eq!(bus.status & ST_ERR, 0);
        assert!(bus.take_irq_edge(), "the completion interrupt must fire");
    }

    /// The data-out counterpart: MODE SELECT with a zero-length parameter
    /// list must likewise skip straight to the status phase instead of
    /// wedging on an empty `Transfer::PacketDataOut`.
    #[test]
    fn packet_zero_length_data_out_completes_without_hanging() {
        let (mut bus, _path) = atapi_bus(2);
        let mut cdb = [0u8; 12];
        cdb[0] = 0x15; // MODE SELECT(6)
        cdb[4] = 0; // parameter list length 0
        issue_packet(&mut bus, &cdb, 0xFFFE);
        assert_eq!(
            bus.transfer,
            Transfer::None,
            "must not enter a DRQ data-out phase with nothing to transfer"
        );
        assert_eq!(bus.status & ST_DRQ, 0, "DRQ must not stay asserted");
        assert_eq!(bus.status & ST_ERR, 0);
        assert!(bus.take_irq_edge(), "the completion interrupt must fire");
    }

    /// DEVICE RESET (0x08) is ATAPI-only per ATA-4: issuing it against a
    /// plain-ATA slot must abort, matching every other type-specific command.
    #[test]
    fn device_reset_aborts_against_a_plain_ata_slot() {
        let disk_path = temp_disk_image(64);
        let mut bus = AtaBus::new();
        bus.attach_drive(
            0,
            IdeDrive::open(&disk_path, 0, None, 0, crate::diskimage::FileSystem::FFS).unwrap(),
        );
        bus.command(0x08);
        assert_ne!(bus.status & ST_ERR, 0);
        assert_eq!(bus.error, ERR_ABRT);
    }

    /// DEVICE RESET against the ATAPI slot must cancel a hung PACKET
    /// transfer left mid-data-phase (DRQ asserted): this is the driver's
    /// real recovery path, complementary to the zero-length-phase fix above
    /// which stops the hang from happening in the first place for a
    /// well-behaved driver.
    #[test]
    fn device_reset_cancels_a_stuck_packet_transfer() {
        let (mut bus, _path) = atapi_bus(8);
        let cdb = read10_cdb(0, 4);
        issue_packet(&mut bus, &cdb, 512);
        assert!(matches!(bus.transfer, Transfer::PacketDataIn { .. }));
        assert_ne!(bus.status & ST_DRQ, 0, "mid-transfer DRQ must be asserted");

        bus.command(0x08);
        assert_eq!(bus.status & ST_ERR, 0, "DEVICE RESET itself must not abort");
        assert_eq!(bus.transfer, Transfer::None);
        assert_eq!(bus.status & ST_DRQ, 0);

        // A subsequent data-port access must do nothing, not replay stale
        // bytes from the cancelled transfer.
        assert_eq!(bus.data_read_word(), 0);
    }

    /// An ATA-only command aborted against the ATAPI slot must leave the
    /// ATAPI drive's sense data agreeing with the error register: ERR set
    /// with sense key 5 (ILLEGAL REQUEST), not "no sense" (key 0).
    #[test]
    fn ata_command_against_atapi_slot_leaves_matching_sense_data() {
        let (mut bus, _path) = atapi_bus(2);
        bus.sector_count = 1;
        bus.command(0x20); // READ SECTORS: ATA-only
        assert_ne!(bus.status & ST_ERR, 0);
        assert_eq!(bus.error, ERR_ABRT);

        let sense_cdb = request_sense_cdb();
        issue_packet(&mut bus, &sense_cdb, 0xFFFE);
        let sense = packet_read_data(&mut bus);
        assert_eq!(
            sense[2] & 0x0F,
            crate::scsi::SK_ILLEGAL_REQUEST,
            "REQUEST SENSE must agree with the ERR the driver just saw"
        );
    }

    /// Two lide drives on different channels must get distinct DHn names
    /// when both are bare-partition (non-RDB) hardfiles: the DOS device name
    /// a synthesized RDB advertises has to be unique across the whole board,
    /// not just within one channel, or a driver enumerating channel 1 finds
    /// the same DH0/DH1 names channel 0 already claimed.
    #[test]
    fn lide_drives_on_different_channels_get_unique_dh_names() {
        use crate::harddrive::SECTOR_SIZE as HD_SECTOR_SIZE;

        fn bare_partition_image(name: &str) -> PathBuf {
            const CYL_BYTES: usize = 16 * 32 * 512; // RDB_HEADS * RDB_SPT * SECTOR_SIZE
            let mut data = vec![0u8; CYL_BYTES];
            data[..4].copy_from_slice(b"DOS\x01"); // FFS boot block signature
            let path = std::env::temp_dir().join(format!(
                "copperline-ata-test-{}-{}-{name}",
                std::process::id(),
                rand_suffix()
            ));
            std::fs::write(&path, &data).unwrap();
            path
        }

        fn dh_name(drive: &mut IdeDrive) -> String {
            let mut sector = [0u8; HD_SECTOR_SIZE];
            drive.disk.read_sector(1, &mut sector).unwrap();
            assert_eq!(&sector[..4], b"PART");
            let len = sector[36] as usize;
            String::from_utf8(sector[37..37 + len].to_vec()).unwrap()
        }

        // Mirrors emulator.rs's lide drive-attach loop: `idx` (the flat
        // 0..4 slot index) names the drive, `idx % 2` (channel-relative)
        // addresses master/slave -- channel 0's master is idx 0, channel 1's
        // master is idx 2.
        let path0 = bare_partition_image("ch0.hdf");
        let path2 = bare_partition_image("ch1.hdf");
        let mut drive0 =
            IdeDrive::open(&path0, 0, None, 0, crate::diskimage::FileSystem::FFS).unwrap();
        let mut drive2 =
            IdeDrive::open(&path2, 2, None, 0, crate::diskimage::FileSystem::FFS).unwrap();
        let name0 = dh_name(&mut drive0);
        let name2 = dh_name(&mut drive2);
        assert_eq!(name0, "DH0");
        assert_eq!(name2, "DH2");
        assert_ne!(name0, name2, "channel 0 and channel 1 must not collide");

        let _ = std::fs::remove_file(&path0);
        let _ = std::fs::remove_file(&path2);
    }
}
