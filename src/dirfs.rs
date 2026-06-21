// SPDX-License-Identifier: GPL-3.0-or-later

//! Build a bare FFS partition image (a "partition hardfile") from a host
//! directory tree, so a directory can be mounted as a Gayle IDE hard disk
//! with no preparation. The image is built once at startup and lives in
//! memory: the guest sees an ordinary FFS volume and may write to it, but
//! nothing is synced back to the host directory.
//!
//! The on-disk layout is plain AmigaDOS FFS (dostype DOS\x01, 512-byte
//! blocks, 16x32 cylinder geometry to match the RDB the IDE layer
//! synthesizes around bare partition images): boot block, root block in the
//! middle of the partition, bitmap blocks after the root, directory header
//! blocks with hashed name chains, and file header / extension blocks whose
//! data pointers reference raw 512-byte data blocks.

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

const BSIZE: usize = 512;
/// Hash-table / data-pointer entries per header block: BSIZE/4 - 56.
const HT_SIZE: usize = 72;
/// Blocks per cylinder of the 16-surface x 32-sector RDB geometry the IDE
/// layer wraps bare partition images with; the image size must be an exact
/// cylinder count.
const CYL_BLOCKS: usize = 512;
/// Blocks mapped per bitmap block: 127 longs of 32 bits after the checksum.
const BM_BITS_PER_BLOCK: usize = 127 * 32;
/// Bitmap-page pointers held in the root block.
const ROOT_BM_PAGES: usize = 25;
/// Bitmap-page pointers per bitmap extension block (last long is the next
/// extension pointer).
const BMEXT_PAGES: usize = BSIZE / 4 - 1;

const T_HEADER: u32 = 2;
const T_LIST: u32 = 16;
const ST_ROOT: u32 = 1;
const ST_USERDIR: u32 = 2;
const ST_FILE: u32 = 0xFFFF_FFFD; // -3

/// Seconds between the Unix epoch and the AmigaDOS epoch (1978-01-01).
const AMIGA_EPOCH_OFFSET: u64 = 252_460_800;

/// Refuse to build images larger than this; the whole image lives in RAM.
const MAX_IMAGE_BYTES: u64 = 2 << 30;

pub fn build_ffs_image(dir: &Path, volume_name: &str) -> anyhow::Result<Vec<u8>> {
    let entries = scan_tree(dir)?;
    let content_blocks: u64 = 3 + entries.iter().map(EntryPlan::blocks_needed).sum::<u64>();

    // Fixed-point size estimate: bitmap blocks depend on the total size.
    let slack = (content_blocks / 20).max(64);
    let mut total = content_blocks + slack;
    loop {
        let with_meta = content_blocks + slack + bitmap_overhead(total);
        let rounded = with_meta.div_ceil(CYL_BLOCKS as u64) * CYL_BLOCKS as u64;
        if rounded <= total {
            break;
        }
        total = rounded;
    }
    if total * BSIZE as u64 > MAX_IMAGE_BYTES {
        anyhow::bail!(
            "directory {} needs a {} MiB volume; refusing to build an in-memory \
             image larger than {} MiB",
            dir.display(),
            (total * BSIZE as u64) >> 20,
            MAX_IMAGE_BYTES >> 20
        );
    }

    let mut b = Builder::new(total as usize, volume_name);
    b.write_tree(dir, &entries, b.root_key)?;
    Ok(b.finish())
}

/// One directory entry found by the host-tree scan.
struct EntryPlan {
    name: Vec<u8>,
    is_dir: bool,
    size: u64,
    mtime: (u32, u32, u32),
    children: Vec<EntryPlan>,
}

impl EntryPlan {
    fn blocks_needed(&self) -> u64 {
        if self.is_dir {
            1 + self
                .children
                .iter()
                .map(EntryPlan::blocks_needed)
                .sum::<u64>()
        } else {
            let data = self.size.div_ceil(BSIZE as u64);
            let headers = 1 + data.saturating_sub(HT_SIZE as u64).div_ceil(HT_SIZE as u64);
            data + headers
        }
    }
}

fn amiga_datestamp(time: SystemTime) -> (u32, u32, u32) {
    let secs = time
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
        .saturating_sub(AMIGA_EPOCH_OFFSET);
    let days = (secs / 86_400) as u32;
    let mins = (secs % 86_400 / 60) as u32;
    let ticks = (secs % 60) as u32 * 50;
    (days, mins, ticks)
}

/// Recursively scan a host directory, in byte-sorted name order so image
/// builds are deterministic. Entries whose names cannot exist on an FFS
/// volume (over 30 bytes, or containing ':' or '/') and non-file/dir
/// entries (symlinks, sockets) are skipped with a warning.
fn scan_tree(dir: &Path) -> anyhow::Result<Vec<EntryPlan>> {
    let uae_metadata_extension = std::ffi::OsStr::new("uaem");
    let mut out = Vec::new();
    let mut listing: Vec<_> = std::fs::read_dir(dir)
        .map_err(|e| anyhow::anyhow!("reading directory {}: {e}", dir.display()))?
        .filter(|e| {
            e.as_ref()
                .is_ok_and(|e| e.path().extension() != Some(uae_metadata_extension))
        })
        .collect::<Result<_, _>>()
        .map_err(|e| anyhow::anyhow!("reading directory {}: {e}", dir.display()))?;
    listing.sort_by_key(|e| e.file_name().into_encoded_bytes().to_vec());
    for entry in listing {
        let name = entry.file_name().into_encoded_bytes().to_vec();
        if name.is_empty() || name.len() > 30 || name.iter().any(|&c| c == b':' || c == b'/') {
            log::warn!(
                "dirfs: skipping {:?}: name not representable on an Amiga volume",
                entry.path()
            );
            continue;
        }
        let meta = entry
            .metadata()
            .map_err(|e| anyhow::anyhow!("stat {}: {e}", entry.path().display()))?;
        let file_type = entry.file_type()?;
        let mtime = amiga_datestamp(meta.modified().unwrap_or(UNIX_EPOCH));
        if file_type.is_dir() {
            out.push(EntryPlan {
                name,
                is_dir: true,
                size: 0,
                mtime,
                children: scan_tree(&entry.path())?,
            });
        } else if file_type.is_file() {
            out.push(EntryPlan {
                name,
                is_dir: false,
                size: meta.len(),
                mtime,
                children: Vec::new(),
            });
        } else {
            log::warn!(
                "dirfs: skipping {:?}: not a regular file or directory",
                entry.path()
            );
        }
    }
    Ok(out)
}

struct Builder {
    image: Vec<u8>,
    total_blocks: usize,
    /// Block-in-use map mirrored into the on-disk bitmap at finish.
    used: Vec<bool>,
    alloc_cursor: usize,
    root_key: u32,
    bitmap_keys: Vec<u32>,
    bmext_keys: Vec<u32>,
    /// Header blocks whose checksum (at offset 20) is computed at finish,
    /// after all hash-chain inserts have settled.
    meta_blocks: Vec<u32>,
}

impl Builder {
    fn new(total_blocks: usize, volume_name: &str) -> Self {
        let mut b = Builder {
            image: vec![0u8; total_blocks * BSIZE],
            total_blocks,
            used: vec![false; total_blocks],
            alloc_cursor: 2,
            root_key: ((2 + total_blocks - 1) / 2) as u32,
            bitmap_keys: Vec::new(),
            bmext_keys: Vec::new(),
            meta_blocks: Vec::new(),
        };
        // Boot block: dostype only, no boot code (hard-disk partitions boot
        // through the RDB, not boot-block code).
        b.image[..4].copy_from_slice(b"DOS\x01");
        b.used[0] = true;
        b.used[1] = true;
        let root = b.root_key as usize;
        b.used[root] = true;

        // Bitmap blocks (and any extension blocks) directly after the root.
        let bm_blocks = (total_blocks - 2).div_ceil(BM_BITS_PER_BLOCK);
        let bmext_blocks = bm_blocks
            .saturating_sub(ROOT_BM_PAGES)
            .div_ceil(BMEXT_PAGES);
        for i in 0..bm_blocks + bmext_blocks {
            b.used[root + 1 + i] = true;
        }
        let mut next = root as u32 + 1;
        for _ in 0..bm_blocks {
            b.bitmap_keys.push(next);
            next += 1;
        }
        for _ in 0..bmext_blocks {
            b.bmext_keys.push(next);
            next += 1;
        }

        b.write_root(volume_name);
        b
    }

    fn put32(&mut self, block: u32, offset: usize, value: u32) {
        let base = block as usize * BSIZE + offset;
        self.image[base..base + 4].copy_from_slice(&value.to_be_bytes());
    }

    fn get32(&self, block: u32, offset: usize) -> u32 {
        let base = block as usize * BSIZE + offset;
        u32::from_be_bytes(self.image[base..base + 4].try_into().unwrap())
    }

    fn alloc(&mut self) -> anyhow::Result<u32> {
        while self.alloc_cursor < self.total_blocks && self.used[self.alloc_cursor] {
            self.alloc_cursor += 1;
        }
        if self.alloc_cursor >= self.total_blocks {
            anyhow::bail!("dirfs: volume sizing underestimated the directory contents");
        }
        self.used[self.alloc_cursor] = true;
        self.alloc_cursor += 1;
        Ok((self.alloc_cursor - 1) as u32)
    }

    fn write_name(&mut self, block: u32, name: &[u8]) {
        let base = block as usize * BSIZE + (BSIZE - 80);
        self.image[base] = name.len() as u8;
        self.image[base + 1..base + 1 + name.len()].copy_from_slice(name);
    }

    fn write_datestamp(&mut self, block: u32, offset: usize, (days, mins, ticks): (u32, u32, u32)) {
        self.put32(block, offset, days);
        self.put32(block, offset + 4, mins);
        self.put32(block, offset + 8, ticks);
    }

    fn write_root(&mut self, volume_name: &str) {
        let now = amiga_datestamp(SystemTime::now());
        let root = self.root_key;
        self.put32(root, 0, T_HEADER);
        self.put32(root, 12, HT_SIZE as u32);
        self.put32(root, BSIZE - 200, !0); // bm_flag: bitmap valid
        for (i, key) in self
            .bitmap_keys
            .iter()
            .take(ROOT_BM_PAGES)
            .copied()
            .collect::<Vec<_>>()
            .into_iter()
            .enumerate()
        {
            self.put32(root, BSIZE - 196 + i * 4, key);
        }
        if let Some(&first_ext) = self.bmext_keys.first() {
            self.put32(root, BSIZE - 96, first_ext);
        }
        self.write_datestamp(root, BSIZE - 92, now); // last root change
        let name: Vec<u8> = volume_name.bytes().take(30).collect();
        let name = if name.is_empty() {
            b"DH0".to_vec()
        } else {
            name
        };
        self.write_name(root, &name);
        self.write_datestamp(root, BSIZE - 40, now); // last disk change
        self.write_datestamp(root, BSIZE - 28, now); // creation
        self.put32(root, BSIZE - 4, ST_ROOT);
        self.meta_blocks.push(root);
    }

    /// Standard AmigaDOS (non-international) name hash.
    fn dos_hash(name: &[u8]) -> usize {
        let mut h = name.len() as u32;
        for &c in name {
            h = h
                .wrapping_mul(13)
                .wrapping_add(u32::from(c.to_ascii_uppercase()))
                & 0x7FF;
        }
        (h % HT_SIZE as u32) as usize
    }

    /// Link `child` into `parent`'s hash table, chaining any current
    /// occupant of the bucket behind it.
    fn hash_insert(&mut self, parent: u32, child: u32, name: &[u8]) {
        let slot = 24 + Self::dos_hash(name) * 4;
        let head = self.get32(parent, slot);
        self.put32(child, BSIZE - 16, head); // next_hash
        self.put32(parent, slot, child);
    }

    fn write_tree(
        &mut self,
        host_dir: &Path,
        entries: &[EntryPlan],
        parent: u32,
    ) -> anyhow::Result<()> {
        for entry in entries {
            let name_str = String::from_utf8_lossy(&entry.name).into_owned();
            let host_path = host_dir.join(&name_str);
            if entry.is_dir {
                let key = self.alloc()?;
                self.put32(key, 0, T_HEADER);
                self.put32(key, 4, key);
                self.write_datestamp(key, BSIZE - 92, entry.mtime);
                self.write_name(key, &entry.name);
                self.put32(key, BSIZE - 12, parent);
                self.put32(key, BSIZE - 4, ST_USERDIR);
                self.hash_insert(parent, key, &entry.name);
                self.meta_blocks.push(key);
                self.write_tree(&host_path, &entry.children, key)?;
            } else {
                let key = self.write_file(&host_path, entry, parent)?;
                self.hash_insert(parent, key, &entry.name);
            }
        }
        Ok(())
    }

    /// Write a file header (plus extension blocks as needed) and its raw FFS
    /// data blocks; returns the header key.
    fn write_file(
        &mut self,
        host_path: &Path,
        entry: &EntryPlan,
        parent: u32,
    ) -> anyhow::Result<u32> {
        let data = std::fs::read(host_path)
            .map_err(|e| anyhow::anyhow!("reading {}: {e}", host_path.display()))?;
        let header = self.alloc()?;
        self.put32(header, 0, T_HEADER);
        self.put32(header, 4, header);
        self.put32(header, BSIZE - 188, data.len() as u32); // byte_size
        self.write_datestamp(header, BSIZE - 92, entry.mtime);
        self.write_name(header, &entry.name);
        self.put32(header, BSIZE - 12, parent);
        self.put32(header, BSIZE - 4, ST_FILE);
        self.meta_blocks.push(header);

        let mut chunk_block = header;
        let mut in_block = 0usize;
        let mut first_data = 0u32;
        for chunk in data.chunks(BSIZE) {
            if in_block == HT_SIZE {
                // Current header/extension is full: chain a T_LIST block.
                let ext = self.alloc()?;
                self.put32(ext, 0, T_LIST);
                self.put32(ext, 4, ext);
                self.put32(ext, BSIZE - 12, header);
                self.put32(ext, BSIZE - 4, ST_FILE);
                self.put32(chunk_block, BSIZE - 8, ext); // extension link
                self.meta_blocks.push(ext);
                chunk_block = ext;
                in_block = 0;
            }
            let block = self.alloc()?;
            let base = block as usize * BSIZE;
            self.image[base..base + chunk.len()].copy_from_slice(chunk);
            if first_data == 0 {
                first_data = block;
            }
            // Data pointers fill the table backwards from the top.
            self.put32(chunk_block, 24 + (HT_SIZE - 1 - in_block) * 4, block);
            in_block += 1;
            let high_seq = self.get32(chunk_block, 8);
            self.put32(chunk_block, 8, high_seq + 1);
        }
        self.put32(header, 16, first_data);
        Ok(header)
    }

    /// Fill in the block-allocation bitmap and all deferred checksums.
    fn finish(mut self) -> Vec<u8> {
        // Bitmap: one bit per block from block 2; set = free. Bits past the
        // end of the disk stay clear.
        for (i, &key) in self.bitmap_keys.clone().iter().enumerate() {
            for j in 0..BM_BITS_PER_BLOCK {
                let block = 2 + i * BM_BITS_PER_BLOCK + j;
                if block >= self.total_blocks || self.used[block] {
                    continue;
                }
                let base = key as usize * BSIZE + 4 + j / 32 * 4;
                let mut long = u32::from_be_bytes(self.image[base..base + 4].try_into().unwrap());
                long |= 1 << (j % 32);
                self.image[base..base + 4].copy_from_slice(&long.to_be_bytes());
            }
            // Bitmap checksum lives in the first long: whole block sums to 0.
            let sum = self.block_sum(key, None);
            self.put32(key, 0, 0u32.wrapping_sub(sum));
        }
        // Bitmap extension chain for pages 25..; no checksum in these.
        for (i, &ext) in self.bmext_keys.clone().iter().enumerate() {
            let pages: Vec<u32> = self
                .bitmap_keys
                .iter()
                .skip(ROOT_BM_PAGES + i * BMEXT_PAGES)
                .take(BMEXT_PAGES)
                .copied()
                .collect();
            for (j, key) in pages.into_iter().enumerate() {
                self.put32(ext, j * 4, key);
            }
            if let Some(&next) = self.bmext_keys.get(i + 1) {
                self.put32(ext, BSIZE - 4, next);
            }
        }
        for key in self.meta_blocks.clone() {
            let sum = self.block_sum(key, Some(20));
            self.put32(key, 20, 0u32.wrapping_sub(sum));
        }
        self.image
    }

    /// Sum of a block's big-endian longs, optionally treating the checksum
    /// long at `skip` as zero.
    fn block_sum(&self, block: u32, skip: Option<usize>) -> u32 {
        let base = block as usize * BSIZE;
        (0..BSIZE / 4)
            .filter(|&i| skip != Some(i * 4))
            .map(|i| {
                u32::from_be_bytes(
                    self.image[base + i * 4..base + i * 4 + 4]
                        .try_into()
                        .unwrap(),
                )
            })
            .fold(0u32, |a, v| a.wrapping_add(v))
    }
}

/// Bitmap blocks plus bitmap-extension blocks needed for `total` blocks.
fn bitmap_overhead(total: u64) -> u64 {
    let bm = (total - 2).div_ceil(BM_BITS_PER_BLOCK as u64);
    bm + bm
        .saturating_sub(ROOT_BM_PAGES as u64)
        .div_ceil(BMEXT_PAGES as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal FFS reader used to verify built images.
    struct Reader<'a> {
        image: &'a [u8],
    }

    impl<'a> Reader<'a> {
        fn get32(&self, block: u32, offset: usize) -> u32 {
            let base = block as usize * BSIZE + offset;
            u32::from_be_bytes(self.image[base..base + 4].try_into().unwrap())
        }

        fn root_key(&self) -> u32 {
            ((2 + self.image.len() / BSIZE - 1) / 2) as u32
        }

        fn checksum_ok(&self, block: u32, at: usize) -> bool {
            let base = block as usize * BSIZE;
            (0..BSIZE / 4)
                .map(|i| {
                    u32::from_be_bytes(
                        self.image[base + i * 4..base + i * 4 + 4]
                            .try_into()
                            .unwrap(),
                    )
                })
                .fold(0u32, |a, v| a.wrapping_add(v))
                == 0
                || at == usize::MAX
        }

        fn name_of(&self, block: u32) -> Vec<u8> {
            let base = block as usize * BSIZE + (BSIZE - 80);
            let len = self.image[base] as usize;
            self.image[base + 1..base + 1 + len].to_vec()
        }

        /// Look `name` up in directory `dir` through the hash chain.
        fn lookup(&self, dir: u32, name: &[u8]) -> Option<u32> {
            let mut key = self.get32(dir, 24 + Builder::dos_hash(name) * 4);
            while key != 0 {
                if self.name_of(key).eq_ignore_ascii_case(name) {
                    return Some(key);
                }
                key = self.get32(key, BSIZE - 16);
            }
            None
        }

        /// Read a whole file back through header/extension data pointers.
        fn read_file(&self, header: u32) -> Vec<u8> {
            let size = self.get32(header, BSIZE - 188) as usize;
            let mut out = Vec::with_capacity(size);
            let mut block = header;
            loop {
                let count = self.get32(block, 8) as usize;
                for i in 0..count {
                    let data = self.get32(block, 24 + (HT_SIZE - 1 - i) * 4);
                    let base = data as usize * BSIZE;
                    let take = (size - out.len()).min(BSIZE);
                    out.extend_from_slice(&self.image[base..base + take]);
                }
                block = self.get32(block, BSIZE - 8);
                if block == 0 {
                    break;
                }
            }
            assert_eq!(out.len(), size);
            out
        }
    }

    fn temp_tree() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "copperline-dirfs-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(dir.join("Sub/Deeper")).unwrap();
        std::fs::write(dir.join("ReadMe.txt"), b"hello amiga\n").unwrap();
        std::fs::write(dir.join("Sub/data.bin"), vec![0xA7u8; 100_000]).unwrap();
        std::fs::write(dir.join("Sub/Deeper/empty"), b"").unwrap();
        dir
    }

    #[test]
    fn builds_a_valid_ffs_volume_from_a_directory() {
        let dir = temp_tree();
        let image = build_ffs_image(&dir, "TestVol").unwrap();
        assert_eq!(image.len() % (CYL_BLOCKS * BSIZE), 0);
        assert_eq!(&image[..4], b"DOS\x01");

        let r = Reader { image: &image };
        let root = r.root_key();
        assert!(r.checksum_ok(root, 20), "root checksum");
        assert_eq!(r.get32(root, 0), T_HEADER);
        assert_eq!(r.get32(root, BSIZE - 4), ST_ROOT);
        assert_eq!(r.name_of(root), b"TestVol");
        assert_eq!(r.get32(root, BSIZE - 200), !0, "bitmap valid flag");
        let bm0 = r.get32(root, BSIZE - 196);
        assert!(bm0 != 0 && r.checksum_ok(bm0, 0), "first bitmap block");

        // Top-level file: content round-trips.
        let readme = r.lookup(root, b"ReadMe.txt").expect("ReadMe.txt");
        assert!(r.checksum_ok(readme, 20));
        assert_eq!(r.get32(readme, BSIZE - 4), ST_FILE);
        assert_eq!(r.read_file(readme), b"hello amiga\n");

        // Nested directory chain and a file large enough to need extension
        // blocks (100000 bytes = 196 data blocks > 72).
        let sub = r.lookup(root, b"Sub").expect("Sub");
        assert_eq!(r.get32(sub, BSIZE - 4), ST_USERDIR);
        assert_eq!(r.get32(sub, BSIZE - 12), root, "parent pointer");
        let data = r.lookup(sub, b"data.bin").expect("data.bin");
        assert_ne!(r.get32(data, BSIZE - 8), 0, "has extension block");
        assert_eq!(r.read_file(data), vec![0xA7u8; 100_000]);

        let deeper = r.lookup(sub, b"Deeper").expect("Deeper");
        let empty = r.lookup(deeper, b"empty").expect("empty");
        assert_eq!(r.get32(empty, 8), 0, "no data blocks");
        assert!(r.read_file(empty).is_empty());

        std::fs::remove_dir_all(&dir).ok();
    }

    /// Utility for cross-checking against external FFS tools (e.g.
    /// amitools' xdftool): builds an image from COPPERLINE_DIRFS_SRC and
    /// writes it to COPPERLINE_DIRFS_OUT.
    #[test]
    #[ignore]
    fn dump_directory_image() {
        let src = crate::envcfg::var("COPPERLINE_DIRFS_SRC").expect("COPPERLINE_DIRFS_SRC");
        let out = crate::envcfg::var("COPPERLINE_DIRFS_OUT").expect("COPPERLINE_DIRFS_OUT");
        let image = build_ffs_image(Path::new(&src), "DirVolume").unwrap();
        std::fs::write(&out, image).unwrap();
        eprintln!("wrote {out}");
    }

    #[test]
    fn bitmap_marks_metadata_used_and_tail_free() {
        let dir = temp_tree();
        let image = build_ffs_image(&dir, "TestVol").unwrap();
        let r = Reader { image: &image };
        let root = r.root_key();
        let bm0 = r.get32(root, BSIZE - 196);
        let bit = |block: u32| -> bool {
            let j = block as usize - 2; // first bitmap block covers 2..
            let base = bm0 as usize * BSIZE + 4 + j / 32 * 4;
            let long = u32::from_be_bytes(image[base..base + 4].try_into().unwrap());
            long & (1 << (j % 32)) != 0
        };
        assert!(!bit(2), "first content block is allocated");
        assert!(!bit(root), "root is allocated");
        assert!(!bit(bm0), "bitmap block itself is allocated");
        // The last block of the slack area is free.
        assert!(bit((image.len() / BSIZE - 1) as u32));
        std::fs::remove_dir_all(&dir).ok();
    }
}
