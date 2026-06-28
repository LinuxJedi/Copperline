// SPDX-License-Identifier: GPL-3.0-or-later

//! Shared hard-drive image backend: the sector store behind both the Gayle
//! IDE drives and the A2091 SCSI targets.
//!
//! A unit opens from a raw HDF image file or from a host directory (built
//! into an in-memory FFS volume at open time, see `dirfs.rs`). Bare
//! partition hardfiles (a filesystem boot block at sector 0, no RDSK) get a
//! synthesized RDB cylinder prepended so the ROM boot driver can
//! mount them without pre-conversion.

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

pub const SECTOR_SIZE: usize = 512;

// HDToolBox-default RDB geometry: 16 surfaces x 32 sectors. Used both for
// the IDE IDENTIFY translation default and for the RDB synthesized in front
// of bare partition hardfiles.
pub const RDB_HEADS: u32 = 16;
pub const RDB_SPT: u32 = 32;
/// Sectors in one cylinder of the RDB geometry (also the size of the
/// synthesized RDB area, which occupies cylinder 0).
pub const CYL_SECTORS: u32 = RDB_HEADS * RDB_SPT;
pub const CYL_BYTES: u64 = CYL_SECTORS as u64 * SECTOR_SIZE as u64;
/// The RDSK block may live in any of the first 16 sectors (RDB_LOCATION_LIMIT).
const RDB_LOCATION_LIMIT: usize = 16;

/// Sector storage behind a drive: a host image file, or an in-memory image
/// (a filesystem built from a host directory) whose writes last only for
/// the session.
enum Backing {
    File(File),
    Memory(Vec<u8>),
}

pub struct HardDriveImage {
    path: PathBuf,
    backing: Backing,
    total_sectors: u64,
    /// Synthesized RDB cylinder prepended to a bare partition hardfile (an
    /// image whose boot block starts with 'DOS\xNN' and that carries no RDSK
    /// of its own). LBAs inside it are served from this buffer and the file
    /// shifts up by one cylinder; writes to it stay in memory for the
    /// session. None for images that contain their own RDB.
    rdb_overlay: Option<Vec<u8>>,
    overlay_write_warned: bool,
    /// Lowercase bus tag ("ide"/"scsi") for log messages.
    bus_name: &'static str,
}

/// Serde shadow of `HardDriveImage`. File-backed images store only the host
/// path (the sectors live in the file, which a save state does not capture);
/// in-memory directory volumes are embedded whole so their session-only
/// writes survive the round trip. Deserialization reopens file backings,
/// which makes a missing/moved image a load-time error instead of a later
/// I/O panic.
#[derive(serde::Serialize, serde::Deserialize)]
struct HardDriveImageState {
    path: PathBuf,
    memory: Option<Vec<u8>>,
    total_sectors: u64,
    rdb_overlay: Option<Vec<u8>>,
    overlay_write_warned: bool,
    scsi_bus: bool,
}

impl serde::Serialize for HardDriveImage {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        HardDriveImageState {
            path: self.path.clone(),
            memory: match &self.backing {
                Backing::Memory(image) => Some(image.clone()),
                Backing::File(_) => None,
            },
            total_sectors: self.total_sectors,
            rdb_overlay: self.rdb_overlay.clone(),
            overlay_write_warned: self.overlay_write_warned,
            scsi_bus: self.bus_name == "scsi",
        }
        .serialize(serializer)
    }
}

impl<'de> serde::Deserialize<'de> for HardDriveImage {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let state = HardDriveImageState::deserialize(deserializer)?;
        let backing = match state.memory {
            Some(image) => Backing::Memory(image),
            None => Backing::File(
                OpenOptions::new()
                    .read(true)
                    .write(true)
                    .open(&state.path)
                    .map_err(|e| {
                        serde::de::Error::custom(format!(
                            "reopening hard-drive image {}: {e}",
                            state.path.display()
                        ))
                    })?,
            ),
        };
        Ok(Self {
            path: state.path,
            backing,
            total_sectors: state.total_sectors,
            rdb_overlay: state.rdb_overlay,
            overlay_write_warned: state.overlay_write_warned,
            bus_name: if state.scsi_bus { "scsi" } else { "ide" },
        })
    }
}

fn put_be32(block: &mut [u8], offset: usize, value: u32) {
    block[offset..offset + 4].copy_from_slice(&value.to_be_bytes());
}

/// Set the longword at offset 8 so the first 64 big-endian longs sum to 0,
/// the checksum rule shared by RDSK and PART blocks.
fn rdb_checksum(block: &mut [u8]) {
    put_be32(block, 8, 0);
    let mut sum = 0u32;
    for i in 0..64 {
        sum = sum.wrapping_add(u32::from_be_bytes(
            block[i * 4..i * 4 + 4].try_into().unwrap(),
        ));
    }
    put_be32(block, 8, 0u32.wrapping_sub(sum));
}

/// Build the RDSK block for a synthesized RDB: `total_cyls` cylinders of
/// 16x32 geometry, RDB occupying cylinder 0, partitionable space from
/// cylinder 1. `disk_label` fills the vendor/product identity fields.
fn build_rdsk_block(total_cyls: u32, disk_label: &str) -> Vec<u8> {
    let mut b = vec![0u8; SECTOR_SIZE];
    b[0..4].copy_from_slice(b"RDSK");
    put_be32(&mut b, 4, 64); // size in longs
    put_be32(&mut b, 12, 7); // host id
    put_be32(&mut b, 16, SECTOR_SIZE as u32);
    put_be32(&mut b, 20, 0x17); // flags: last disk/LUN/ID
    put_be32(&mut b, 24, !0); // bad-block list: none
    put_be32(&mut b, 28, 1); // partition list at sector 1
    put_be32(&mut b, 32, !0); // filesystem-header list: none
    put_be32(&mut b, 36, !0); // drive init
    put_be32(&mut b, 40, !0);
    for off in (44..64).step_by(4) {
        put_be32(&mut b, off, !0);
    }
    put_be32(&mut b, 64, total_cyls);
    put_be32(&mut b, 68, RDB_SPT);
    put_be32(&mut b, 72, RDB_HEADS);
    put_be32(&mut b, 76, 1); // interleave
    put_be32(&mut b, 80, total_cyls); // park cylinder
    put_be32(&mut b, 96, !0); // write precomp
    put_be32(&mut b, 100, !0); // reduced write
    put_be32(&mut b, 104, 3); // step rate
    put_be32(&mut b, 128, 0); // rdb blocks low
    put_be32(&mut b, 132, CYL_SECTORS - 1); // rdb blocks high
    put_be32(&mut b, 136, 1); // lo cylinder
    put_be32(&mut b, 140, total_cyls - 1); // hi cylinder
    put_be32(&mut b, 144, CYL_SECTORS); // blocks per cylinder
                                        // Vendor (8) + product (16) identity, then the revision.
    let mut label = disk_label.as_bytes().to_vec();
    label.resize(24, b' ');
    b[160..184].copy_from_slice(&label);
    b[184..188].copy_from_slice(b"1.0 ");
    rdb_checksum(&mut b);
    b
}

/// Build the PART block: one bootable partition (device `name`, e.g. DH0)
/// spanning cylinders 1..total_cyls-1 with the dostype found in the image's
/// boot block.
fn build_part_block(total_cyls: u32, dostype: u32, name: &[u8]) -> Vec<u8> {
    let mut b = vec![0u8; SECTOR_SIZE];
    b[0..4].copy_from_slice(b"PART");
    put_be32(&mut b, 4, 64);
    put_be32(&mut b, 12, 7); // host id
    put_be32(&mut b, 16, !0); // next partition: none
    put_be32(&mut b, 20, 1); // flags: bootable
    let name = &name[..name.len().min(31)];
    b[36] = name.len() as u8;
    b[37..37 + name.len()].copy_from_slice(name);
    let env: [u32; 17] = [
        16,                       // table size
        (SECTOR_SIZE / 4) as u32, // longs per block
        0,                        // sec org
        RDB_HEADS,
        1, // sectors per block
        RDB_SPT,
        2, // DOS reserved blocks
        0, // prealloc
        0, // interleave
        1, // low cylinder
        total_cyls - 1,
        30,          // buffers
        0,           // buffer memory type
        0x00FF_FFFF, // max transfer
        0x7FFF_FFFE, // mask
        0,           // boot priority
        dostype,
    ];
    for (i, v) in env.iter().enumerate() {
        put_be32(&mut b, 128 + i * 4, *v);
    }
    rdb_checksum(&mut b);
    b
}

impl HardDriveImage {
    /// Open a drive image. `device_name` is the DOS device name (e.g.
    /// "DH0") a synthesized RDB advertises; `bus_name` ("ide"/"scsi") tags
    /// log messages; `disk_label` fills the RDSK vendor/product identity.
    /// The path may be a raw HDF image file, or a host directory, which is
    /// built into an in-memory FFS volume at open time. `volume_override`
    /// names that FFS volume; when `None` the directory name is used. It has
    /// no effect on a raw HDF, which carries its own label inside the image.
    pub fn open(
        path: &Path,
        device_name: &str,
        bus_name: &'static str,
        disk_label: &str,
        volume_override: Option<&str>,
    ) -> anyhow::Result<Self> {
        let mut backing = if path.is_dir() {
            let volume = volume_override.map(str::to_string).unwrap_or_else(|| {
                path.file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default()
            });
            let image = crate::dirfs::build_ffs_image(path, &volume)?;
            log::warn!(
                "{bus_name}: {} mounted as an in-memory volume \"{volume}\" built from the \
                 directory; guest writes to it are NOT written back to the host and are lost \
                 at exit",
                path.display()
            );
            Backing::Memory(image)
        } else {
            if volume_override.is_some() {
                log::warn!(
                    "{bus_name}: {} is a raw HDF image; the configured drive name is ignored \
                     (the volume label lives inside the image)",
                    path.display()
                );
            }
            Backing::File(
                OpenOptions::new()
                    .read(true)
                    .write(true)
                    .open(path)
                    .map_err(|e| {
                        anyhow::anyhow!("opening {bus_name} image {}: {e}", path.display())
                    })?,
            )
        };
        let len = match &backing {
            Backing::File(file) => file
                .metadata()
                .map_err(|e| anyhow::anyhow!("stat {bus_name} image {}: {e}", path.display()))?
                .len(),
            Backing::Memory(image) => image.len() as u64,
        };
        if len == 0 || len % SECTOR_SIZE as u64 != 0 {
            anyhow::bail!(
                "{bus_name} image {} is {} bytes; expected a non-empty multiple of {} (raw HDF)",
                path.display(),
                len,
                SECTOR_SIZE
            );
        }

        // Sniff the first sectors: a bare partition hardfile (filesystem boot
        // block at sector 0, no RDSK block in the first 16 sectors) gets a
        // synthesized RDB cylinder in front so the driver can mount it
        // without any pre-conversion step.
        let sniff_len = (len as usize).min(RDB_LOCATION_LIMIT * SECTOR_SIZE);
        let mut head = vec![0u8; sniff_len];
        match &mut backing {
            Backing::File(file) => file
                .read_exact(&mut head)
                .map_err(|e| anyhow::anyhow!("reading {bus_name} image {}: {e}", path.display()))?,
            Backing::Memory(image) => head.copy_from_slice(&image[..sniff_len]),
        }
        let has_rdsk = head
            .chunks(SECTOR_SIZE)
            .any(|sector| sector.get(..4) == Some(b"RDSK"));
        let bare_partition = !has_rdsk && head.get(..3) == Some(b"DOS");

        let rdb_overlay = if bare_partition {
            if len % CYL_BYTES != 0 {
                anyhow::bail!(
                    "{bus_name} image {} looks like a bare partition hardfile ({} bytes) but is \
                     not a multiple of {} (16 surfaces x 32 sectors); cannot wrap it in \
                     an RDB without guessing geometry",
                    path.display(),
                    len,
                    CYL_BYTES
                );
            }
            let part_cyls = (len / CYL_BYTES) as u32;
            let total_cyls = 1 + part_cyls;
            let dostype = u32::from_be_bytes(head[..4].try_into().unwrap());
            let mut overlay = vec![0u8; CYL_BYTES as usize];
            overlay[..SECTOR_SIZE].copy_from_slice(&build_rdsk_block(total_cyls, disk_label));
            overlay[SECTOR_SIZE..2 * SECTOR_SIZE].copy_from_slice(&build_part_block(
                total_cyls,
                dostype,
                device_name.as_bytes(),
            ));
            log::info!(
                "{bus_name}: {} is a bare partition hardfile (dostype {:08X}); wrapping it in \
                 a synthesized RDB ({} cylinders, partition {} on 1-{})",
                path.display(),
                dostype,
                total_cyls,
                device_name,
                total_cyls - 1
            );
            Some(overlay)
        } else {
            None
        };

        let overlay_sectors = rdb_overlay.as_ref().map_or(0, |_| u64::from(CYL_SECTORS));
        let total_sectors = len / SECTOR_SIZE as u64 + overlay_sectors;
        Ok(Self {
            path: path.to_path_buf(),
            backing,
            total_sectors,
            rdb_overlay,
            overlay_write_warned: false,
            bus_name,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn total_sectors(&self) -> u64 {
        self.total_sectors
    }

    /// Sectors served from the synthesized RDB overlay; file LBAs shift down
    /// by this much.
    fn overlay_sectors(&self) -> u64 {
        self.rdb_overlay
            .as_ref()
            .map_or(0, |_| u64::from(CYL_SECTORS))
    }

    pub fn read_sector(&mut self, lba: u64, buf: &mut [u8]) -> std::io::Result<()> {
        if let Some(overlay) = &self.rdb_overlay {
            if lba < u64::from(CYL_SECTORS) {
                let off = lba as usize * SECTOR_SIZE;
                buf[..SECTOR_SIZE].copy_from_slice(&overlay[off..off + SECTOR_SIZE]);
                return Ok(());
            }
        }
        let file_lba = lba - self.overlay_sectors();
        match &mut self.backing {
            Backing::File(file) => {
                file.seek(SeekFrom::Start(file_lba * SECTOR_SIZE as u64))?;
                file.read_exact(&mut buf[..SECTOR_SIZE])
            }
            Backing::Memory(image) => {
                let off = file_lba as usize * SECTOR_SIZE;
                buf[..SECTOR_SIZE].copy_from_slice(&image[off..off + SECTOR_SIZE]);
                Ok(())
            }
        }
    }

    pub fn write_sector(&mut self, lba: u64, buf: &[u8]) -> std::io::Result<()> {
        if let Some(overlay) = &mut self.rdb_overlay {
            if lba < u64::from(CYL_SECTORS) {
                if !self.overlay_write_warned {
                    self.overlay_write_warned = true;
                    log::warn!(
                        "{}: {}: write to the synthesized RDB area; partition-table \
                         changes are kept in memory and will be lost at exit",
                        self.bus_name,
                        self.path.display()
                    );
                }
                let off = lba as usize * SECTOR_SIZE;
                overlay[off..off + SECTOR_SIZE].copy_from_slice(&buf[..SECTOR_SIZE]);
                return Ok(());
            }
        }
        let file_lba = lba - self.overlay_sectors();
        match &mut self.backing {
            Backing::File(file) => {
                file.seek(SeekFrom::Start(file_lba * SECTOR_SIZE as u64))?;
                file.write_all(&buf[..SECTOR_SIZE])
            }
            Backing::Memory(image) => {
                let off = file_lba as usize * SECTOR_SIZE;
                image[off..off + SECTOR_SIZE].copy_from_slice(&buf[..SECTOR_SIZE]);
                Ok(())
            }
        }
    }

    pub fn flush(&mut self) {
        if let Backing::File(file) = &mut self.backing {
            if let Err(e) = file.flush() {
                log::warn!(
                    "{} image {}: flush failed: {e}",
                    self.bus_name,
                    self.path.display()
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_image(name: &str, bytes: &[u8]) -> PathBuf {
        static UNIQUE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let unique = UNIQUE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "copperline-harddrive-{}-{unique}-{name}",
            std::process::id()
        ));
        std::fs::write(&path, bytes).unwrap();
        path
    }

    #[test]
    fn serde_reopens_file_backed_image_and_serves_same_sectors() {
        let mut bytes = vec![0u8; 64 * SECTOR_SIZE];
        bytes[5 * SECTOR_SIZE..5 * SECTOR_SIZE + 4].copy_from_slice(b"MARK");
        let path = temp_image("serde.hdf", &bytes);
        let mut original = HardDriveImage::open(&path, "DH0", "ide", "TEST DISK", None).unwrap();

        let encoded = bincode::serialize(&original).unwrap();
        let mut restored: HardDriveImage = bincode::deserialize(&encoded).unwrap();
        assert_eq!(restored.total_sectors(), original.total_sectors());
        assert_eq!(restored.path(), original.path());
        let mut a = vec![0u8; SECTOR_SIZE];
        let mut b = vec![0u8; SECTOR_SIZE];
        for lba in [0u64, 5, 63, original.total_sectors() - 1] {
            original.read_sector(lba, &mut a).unwrap();
            restored.read_sector(lba, &mut b).unwrap();
            assert_eq!(a, b, "lba {lba}");
        }

        // Writes through the restored handle land in the same backing file.
        let sector = vec![0x5A; SECTOR_SIZE];
        restored.write_sector(7, &sector).unwrap();
        restored.flush();
        original.read_sector(7, &mut a).unwrap();
        assert_eq!(a, sector);

        let _ = std::fs::remove_file(&path);
    }

    /// Read every sector of an image and return the FFS volume label found in
    /// its root block (block type 2 / secondary type 1). The directory mount
    /// prepends a synthesized RDB, so the root block is not at a fixed LBA;
    /// scan for it instead.
    fn volume_label(disk: &mut HardDriveImage) -> String {
        let mut sector = vec![0u8; SECTOR_SIZE];
        for lba in 0..disk.total_sectors() {
            disk.read_sector(lba, &mut sector).unwrap();
            let be32 = |off: usize| u32::from_be_bytes(sector[off..off + 4].try_into().unwrap());
            if be32(0) == 2 && be32(SECTOR_SIZE - 4) == 1 {
                let name_base = SECTOR_SIZE - 80;
                let len = sector[name_base] as usize;
                return String::from_utf8_lossy(&sector[name_base + 1..name_base + 1 + len])
                    .into_owned();
            }
        }
        panic!("no FFS root block found");
    }

    /// A uniquely-named parent temp dir holding a short-named child directory
    /// (returned) with one file. The child's name is short enough that the
    /// 30-character FFS volume-label limit does not truncate it.
    fn temp_mount_dir(child: &str) -> (PathBuf, PathBuf) {
        static UNIQUE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let unique = UNIQUE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let parent = std::env::temp_dir().join(format!(
            "copperline-harddrive-dir-{}-{unique}",
            std::process::id()
        ));
        let mount = parent.join(child);
        std::fs::create_dir_all(&mount).unwrap();
        std::fs::write(mount.join("hello.txt"), b"hi\n").unwrap();
        (parent, mount)
    }

    #[test]
    fn directory_mount_uses_the_directory_name_as_the_volume_label() {
        let (parent, mount) = temp_mount_dir("Games");
        let mut disk = HardDriveImage::open(&mount, "DH0", "ide", "TEST DISK", None).unwrap();
        assert_eq!(volume_label(&mut disk), "Games");
        let _ = std::fs::remove_dir_all(&parent);
    }

    #[test]
    fn directory_mount_volume_override_sets_the_label() {
        let (parent, mount) = temp_mount_dir("Games");
        let mut disk =
            HardDriveImage::open(&mount, "DH0", "ide", "TEST DISK", Some("Workbench")).unwrap();
        assert_eq!(volume_label(&mut disk), "Workbench");
        let _ = std::fs::remove_dir_all(&parent);
    }

    #[test]
    fn serde_errors_when_backing_file_is_missing() {
        let path = temp_image("gone.hdf", &vec![0u8; 8 * SECTOR_SIZE]);
        let original = HardDriveImage::open(&path, "DH0", "ide", "TEST DISK", None).unwrap();
        let encoded = bincode::serialize(&original).unwrap();
        let _ = std::fs::remove_file(&path);
        let Err(err) = bincode::deserialize::<HardDriveImage>(&encoded) else {
            panic!("deserializing with the image file gone must fail");
        };
        assert!(err.to_string().contains("reopening hard-drive image"));
    }
}
