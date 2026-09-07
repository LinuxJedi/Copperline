// SPDX-License-Identifier: GPL-3.0-or-later

//! Build a bare partition image (a "partition hardfile") from a host
//! directory tree, so a directory can be mounted as a Gayle IDE hard disk
//! with no preparation. The image is built once at startup and lives in
//! memory: the guest sees an ordinary AmigaDOS volume and may write to it,
//! but nothing is synced back to the host directory.
//!
//! The on-disk layout is plain AmigaDOS FFS or OFS (dostype DOS\x01 or
//! DOS\x00, caller's choice; see [`build_image`]), 512-byte blocks, 16x32
//! cylinder geometry to match the RDB the IDE layer synthesizes around bare
//! partition images: boot block, root block in the middle of the partition,
//! bitmap blocks after the root, directory header blocks with hashed name
//! chains, and file header / extension blocks whose data pointers reference
//! either raw 512-byte FFS data blocks or 24-byte-header OFS data blocks
//! (488 payload bytes apiece, chained through their own `next_data`
//! pointers). OFS exists alongside FFS because Kickstart 1.3's ROM has no
//! FFS handler built in -- OFS is the only filesystem a 1.3 guest can read
//! from a directory mount with no guest-side setup.

use std::ffi::{OsStr, OsString};
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
const T_DATA: u32 = 8;
const ST_ROOT: u32 = 1;
const ST_USERDIR: u32 = 2;
const ST_FILE: u32 = 0xFFFF_FFFD; // -3

/// Seconds between the Unix epoch and the AmigaDOS epoch (1978-01-01).
const AMIGA_EPOCH_OFFSET: u64 = 252_460_800;

/// Refuse to build images larger than this; the whole image lives in RAM.
const MAX_IMAGE_BYTES: u64 = 2 << 30;

/// Data payload bytes per content block: FFS data blocks carry no header
/// (the full 512 bytes are payload); OFS data blocks reserve the first 24
/// bytes for type/header_key/seq_num/data_size/next_data/checksum.
fn data_payload_bytes(filesystem: crate::diskimage::FileSystem) -> u64 {
    if filesystem.ffs {
        BSIZE as u64
    } else {
        (BSIZE - 24) as u64
    }
}

pub fn build_image(
    dir: &Path,
    volume_name: &str,
    filesystem: crate::diskimage::FileSystem,
) -> anyhow::Result<Vec<u8>> {
    build_image_limited(dir, volume_name, filesystem, MAX_IMAGE_BYTES)
}

pub(crate) fn build_image_limited(
    dir: &Path,
    volume_name: &str,
    filesystem: crate::diskimage::FileSystem,
    limit: u64,
) -> anyhow::Result<Vec<u8>> {
    // Only `ffs`/plain-vs-not matters here: Kickstart 1.3 (the reason OFS
    // exists as an option at all) has no intl/dircache/longname support
    // either, so callers are only ever expected to pass FileSystem::OFS or
    // FileSystem::FFS.
    debug_assert!(
        filesystem.variant == crate::diskimage::Variant::Plain,
        "dirfs only builds plain OFS/FFS volumes"
    );
    let entries = scan_tree(dir)?;
    let content_blocks: u64 = 3 + entries
        .iter()
        .map(|e| e.blocks_needed(filesystem))
        .sum::<u64>();

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
    if total * BSIZE as u64 > limit {
        anyhow::bail!(
            "directory {} needs a {} MiB volume; refusing to build an in-memory \
             image larger than {} MiB",
            dir.display(),
            (total * BSIZE as u64) >> 20,
            limit >> 20
        );
    }

    let mut b = Builder::new(total as usize, volume_name, filesystem);
    b.write_tree(dir, &entries, b.root_key)?;
    Ok(b.finish())
}

/// Build a standard DD floppy filesystem from a host directory tree.
///
/// Unlike [`build_image`], whose geometry is a partition hardfile, this pins
/// the volume to 80 tracks x 2 sides x 11 sectors and installs the AmigaDOS
/// floppy boot code when requested. The directory/hash/file writer is shared,
/// so exported ADFs and directory-backed hardfiles cannot drift in their OFS
/// layout.
pub fn build_floppy_image(
    dir: &Path,
    volume_name: &str,
    filesystem: crate::diskimage::FileSystem,
    bootable: bool,
) -> anyhow::Result<Vec<u8>> {
    debug_assert!(
        filesystem.variant == crate::diskimage::Variant::Plain,
        "dirfs only builds plain OFS/FFS volumes"
    );
    let entries = scan_tree(dir)?;
    let total_blocks = crate::diskimage::FLOPPY_DD_BLOCKS as usize;
    let content_blocks: u64 = 3 + entries
        .iter()
        .map(|entry| entry.blocks_needed(filesystem))
        .sum::<u64>();
    let available = total_blocks as u64 - bitmap_overhead(total_blocks as u64);
    if content_blocks > available {
        anyhow::bail!(
            "directory {} does not fit on a DD floppy (needs at least {} content blocks, {} available)",
            dir.display(),
            content_blocks,
            available
        );
    }
    let mut builder = Builder::new(total_blocks, volume_name, filesystem);
    let root_key = builder.root_key;
    builder.write_tree(dir, &entries, root_key)?;
    let root = u64::from(builder.root_key);
    let mut image = builder.finish();
    image[..BSIZE].copy_from_slice(&crate::diskimage::boot_block(filesystem, bootable, root));
    image[BSIZE..BSIZE * 2].fill(0);
    Ok(image)
}

/// One directory entry found by the host-tree scan.
struct EntryPlan {
    name: Vec<u8>,
    name_osstring: OsString,
    is_dir: bool,
    size: u64,
    mtime: (u32, u32, u32),
    children: Vec<EntryPlan>,
}

impl EntryPlan {
    fn blocks_needed(&self, filesystem: crate::diskimage::FileSystem) -> u64 {
        if self.is_dir {
            1 + self
                .children
                .iter()
                .map(|c| c.blocks_needed(filesystem))
                .sum::<u64>()
        } else {
            let data = self.size.div_ceil(data_payload_bytes(filesystem));
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

fn osstring_to_latin1(os: &OsStr) -> Option<Vec<u8>> {
    if os.is_empty() {
        log::warn!("dirfs: skipping entry with empty name");
        return None;
    }
    let s: &str = os
        .try_into()
        .map_err(|_| log::warn!("dirfs: failed to convert \"{}\" to utf8", os.display()))
        .ok()?;
    let s_len = s.len();
    if s_len > 30 {
        log::warn!(
            "dirfs: skipping entry \"{}\" with more than 30 characters",
            os.display()
        );
        return None;
    }
    let mut result = Vec::with_capacity(s_len);
    for ch in s.chars() {
        let code = ch as u32;
        if code <= 0xFF {
            let code_u8 = code as u8;
            if code_u8 == b':' || code_u8 == b'/' {
                log::warn!(
                    "dirfs: skipping entry \"{}\" with invalid character(s)",
                    os.display()
                );
                return None;
            } else {
                result.push(code_u8);
            };
        } else {
            log::warn!("dirfs: failed to convert char \"{ch}\" to latin1");
            return None;
        }
    }
    Some(result)
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
        let name_osstring = entry.file_name();
        let name = match osstring_to_latin1(&name_osstring) {
            Some(n) => n,
            None => continue,
        };
        let meta = entry
            .metadata()
            .map_err(|e| anyhow::anyhow!("stat {}: {e}", entry.path().display()))?;
        let file_type = entry.file_type()?;
        let mtime = amiga_datestamp(meta.modified().unwrap_or(UNIX_EPOCH));
        if file_type.is_dir() {
            out.push(EntryPlan {
                name,
                name_osstring,
                is_dir: true,
                size: 0,
                mtime,
                children: scan_tree(&entry.path())?,
            });
        } else if file_type.is_file() {
            out.push(EntryPlan {
                name,
                name_osstring,
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
    /// after all hash-chain inserts have settled. OFS data blocks are pushed
    /// here too: they carry a checksum at the same offset, computed the
    /// same way.
    meta_blocks: Vec<u32>,
    filesystem: crate::diskimage::FileSystem,
}

impl Builder {
    fn new(
        total_blocks: usize,
        volume_name: &str,
        filesystem: crate::diskimage::FileSystem,
    ) -> Self {
        let mut b = Builder {
            image: vec![0u8; total_blocks * BSIZE],
            total_blocks,
            used: vec![false; total_blocks],
            alloc_cursor: 2,
            root_key: ((2 + total_blocks - 1) / 2) as u32,
            bitmap_keys: Vec::new(),
            bmext_keys: Vec::new(),
            meta_blocks: Vec::new(),
            filesystem,
        };
        // Boot block: dostype only, no boot code (hard-disk partitions boot
        // through the RDB, not boot-block code).
        b.image[..4].copy_from_slice(&filesystem.dos_type().to_be_bytes());
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
            let host_path = host_dir.join(&entry.name_osstring);
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

    /// Write a file header (plus extension blocks as needed) and its data
    /// blocks (raw FFS or header-carrying OFS, per `self.filesystem`);
    /// returns the header key.
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
        // OFS-only bookkeeping: the previous data block's key (to backfill
        // its next_data pointer once the current one's key is known) and a
        // running 1-based sequence number.
        let mut prev_data: Option<u32> = None;
        let mut seq_num = 0u32;
        let payload = data_payload_bytes(self.filesystem) as usize;
        for chunk in data.chunks(payload) {
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
            if self.filesystem.ffs {
                let base = block as usize * BSIZE;
                self.image[base..base + chunk.len()].copy_from_slice(chunk);
            } else {
                seq_num += 1;
                self.put32(block, 0, T_DATA);
                self.put32(block, 4, header); // header_key: the file's header, never an extension
                self.put32(block, 8, seq_num);
                self.put32(block, 12, chunk.len() as u32); // data_size
                self.put32(block, 16, 0); // next_data, backfilled below once known
                self.meta_blocks.push(block); // checksum at offset 20, deferred to finish()
                let base = block as usize * BSIZE + 24;
                self.image[base..base + chunk.len()].copy_from_slice(chunk);
                if let Some(prev) = prev_data {
                    self.put32(prev, 16, block);
                }
                prev_data = Some(block);
            }
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

    /// Minimal FFS/OFS reader used to verify built images.
    struct Reader<'a> {
        image: &'a [u8],
        filesystem: crate::diskimage::FileSystem,
    }

    impl<'a> Reader<'a> {
        fn ffs(image: &'a [u8]) -> Self {
            Reader {
                image,
                filesystem: crate::diskimage::FileSystem::FFS,
            }
        }

        fn ofs(image: &'a [u8]) -> Self {
            Reader {
                image,
                filesystem: crate::diskimage::FileSystem::OFS,
            }
        }

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

        /// Read a whole file back through header/extension data pointers. For
        /// OFS, each data pointer names a block with its own 24-byte header
        /// (data_size may be short only on the very last block); the
        /// `next_data` chain is not consulted here since the header's/
        /// extension's own pointer table already lists every data block in
        /// order -- `next_data` exists for OFS readers that don't have that
        /// table (e.g. real AmigaDOS repairing a damaged header), and is
        /// checked separately in ofs-specific tests below.
        fn read_file(&self, header: u32) -> Vec<u8> {
            let size = self.get32(header, BSIZE - 188) as usize;
            let mut out = Vec::with_capacity(size);
            let mut block = header;
            loop {
                let count = self.get32(block, 8) as usize;
                for i in 0..count {
                    let data = self.get32(block, 24 + (HT_SIZE - 1 - i) * 4);
                    if self.filesystem.ffs {
                        let base = data as usize * BSIZE;
                        let take = (size - out.len()).min(BSIZE);
                        out.extend_from_slice(&self.image[base..base + take]);
                    } else {
                        let data_size = self.get32(data, 12) as usize;
                        let base = data as usize * BSIZE + 24;
                        out.extend_from_slice(&self.image[base..base + data_size]);
                    }
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
        std::fs::write(dir.join("spaß"), b"sharp_s").unwrap();
        dir
    }

    #[test]
    fn bootable_floppy_reuses_the_directory_tree_writer() {
        let source = temp_tree();
        let image = build_floppy_image(
            &source,
            "Copperline",
            crate::diskimage::FileSystem::OFS,
            true,
        )
        .expect("floppy image");
        assert_eq!(
            image.len(),
            crate::diskimage::FLOPPY_DD_BLOCKS as usize * BSIZE
        );
        assert_eq!(&image[..4], b"DOS\0");
        assert_ne!(&image[12..32], &[0; 20], "boot code missing");
        let reader = Reader::ofs(&image);
        assert_eq!(reader.root_key(), 880);
        let readme = reader
            .lookup(reader.root_key(), b"ReadMe.txt")
            .expect("root file");
        assert_eq!(reader.read_file(readme), b"hello amiga\n");
        std::fs::remove_dir_all(source).ok();
    }

    #[test]
    fn builds_a_valid_ffs_volume_from_a_directory() {
        let dir = temp_tree();
        let image = build_image(&dir, "TestVol", crate::diskimage::FileSystem::FFS).unwrap();
        assert_eq!(image.len() % (CYL_BLOCKS * BSIZE), 0);
        assert_eq!(&image[..4], b"DOS\x01");

        let r = Reader::ffs(&image);
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

        // Non ASCII char
        let german_fun = OsStr::new("spaß");
        let amiga_fun = osstring_to_latin1(german_fun).expect("OsString conversion");
        assert_eq!(amiga_fun, [b's', b'p', b'a', 0xdf]);
        let spass = r.lookup(root, &amiga_fun).expect("No fun");
        assert_eq!(r.read_file(spass), b"sharp_s");

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

    #[test]
    fn builds_a_valid_ofs_volume_from_a_directory() {
        let dir = temp_tree();
        let image = build_image(&dir, "TestVol", crate::diskimage::FileSystem::OFS).unwrap();
        assert_eq!(image.len() % (CYL_BLOCKS * BSIZE), 0);
        assert_eq!(&image[..4], b"DOS\x00");

        let r = Reader::ofs(&image);
        let root = r.root_key();
        assert!(r.checksum_ok(root, 20), "root checksum");
        assert_eq!(r.name_of(root), b"TestVol");

        // Single small file: one OFS data block whose header fields are
        // right, and whose next_data is 0 (end of chain).
        let readme = r.lookup(root, b"ReadMe.txt").expect("ReadMe.txt");
        assert!(r.checksum_ok(readme, 20));
        let data_ptr = r.get32(readme, 24 + (HT_SIZE - 1) * 4);
        assert_ne!(data_ptr, 0, "ReadMe.txt has a data block");
        assert_eq!(r.get32(data_ptr, 0), T_DATA);
        assert_eq!(
            r.get32(data_ptr, 4),
            readme,
            "header_key points at the file header"
        );
        assert_eq!(r.get32(data_ptr, 8), 1, "seq_num");
        assert_eq!(
            r.get32(data_ptr, 12),
            b"hello amiga\n".len() as u32,
            "data_size"
        );
        assert_eq!(r.get32(data_ptr, 16), 0, "single block: next_data is 0");
        assert!(r.checksum_ok(data_ptr, 20), "data block checksum");
        assert_eq!(r.read_file(readme), b"hello amiga\n");

        // A file spanning multiple, non-block-aligned data blocks: exercise
        // data_size truncation on the last block and the next_data chain.
        let sub = r.lookup(root, b"Sub").expect("Sub");
        let data = r.lookup(sub, b"data.bin").expect("data.bin");
        assert_ne!(
            r.get32(data, BSIZE - 8),
            0,
            "has extension block (>72 OFS data blocks)"
        );
        assert_eq!(r.read_file(data), vec![0xA7u8; 100_000]);
        // Walk the next_data chain directly (rather than through the header's
        // pointer table) to prove it is intact end to end, including across
        // the T_LIST extension boundary.
        let mut block = r.get32(data, 24 + (HT_SIZE - 1) * 4);
        let mut seen = 0u32;
        let mut total = 0usize;
        loop {
            seen += 1;
            assert_eq!(
                r.get32(block, 8),
                seen,
                "seq_num increments across extensions"
            );
            assert_eq!(
                r.get32(block, 4),
                data,
                "header_key stays the original file header"
            );
            total += r.get32(block, 12) as usize;
            let next = r.get32(block, 16);
            if next == 0 {
                break;
            }
            block = next;
        }
        assert_eq!(total, 100_000);
        assert!(seen > HT_SIZE as u32, "the file needed a T_LIST extension");

        let empty_dir = r.lookup(sub, b"Deeper").expect("Deeper");
        let empty = r.lookup(empty_dir, b"empty").expect("empty");
        assert_eq!(r.get32(empty, 8), 0, "no data blocks");
        assert!(r.read_file(empty).is_empty());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn ofs_data_block_content_round_trips_on_a_non_aligned_boundary() {
        // 488*2 + 137 bytes: two full OFS data blocks plus a short last one,
        // chosen to catch a data_size truncation bug on the final block.
        let dir = std::env::temp_dir().join(format!(
            "copperline-dirfs-ofs-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let content: Vec<u8> = (0..(488 * 2 + 137)).map(|i| (i % 251) as u8).collect();
        std::fs::write(dir.join("odd.bin"), &content).unwrap();

        let image = build_image(&dir, "TestVol", crate::diskimage::FileSystem::OFS).unwrap();
        let r = Reader::ofs(&image);
        let root = r.root_key();
        let file = r.lookup(root, b"odd.bin").expect("odd.bin");
        assert_eq!(r.read_file(file), content);

        // The last data block's data_size is the short remainder, not 488.
        let mut block = r.get32(file, 24 + (HT_SIZE - 1) * 4);
        loop {
            let next = r.get32(block, 16);
            if next == 0 {
                assert_eq!(r.get32(block, 12), 137, "last block keeps its short length");
                break;
            }
            block = next;
        }

        std::fs::remove_dir_all(&dir).ok();
    }

    /// Utility for cross-checking against external FFS tools (e.g.
    /// amitools' xdftool): builds an image from COPPERLINE_DIRFS_SRC and
    /// writes it to COPPERLINE_DIRFS_OUT.
    ///
    /// A generator rather than a check, so it skips cleanly when its
    /// inputs are unset: `cargo test -- --ignored` stops at the first
    /// failing binary, and panicking here would hide every asset-gated
    /// integration test behind it (see tests/README.md).
    #[test]
    #[ignore]
    fn dump_directory_image() {
        let (Some(src), Some(out)) = (
            crate::envcfg::var("COPPERLINE_DIRFS_SRC"),
            crate::envcfg::var("COPPERLINE_DIRFS_OUT"),
        ) else {
            eprintln!("skipping: set COPPERLINE_DIRFS_SRC and COPPERLINE_DIRFS_OUT to run it");
            return;
        };
        let image = build_image(
            Path::new(&src),
            "DirVolume",
            crate::diskimage::FileSystem::FFS,
        )
        .unwrap();
        std::fs::write(&out, image).unwrap();
        eprintln!("wrote {out}");
    }

    #[test]
    fn bitmap_marks_metadata_used_and_tail_free() {
        let dir = temp_tree();
        let image = build_image(&dir, "TestVol", crate::diskimage::FileSystem::FFS).unwrap();
        let r = Reader::ffs(&image);
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

    #[test]
    fn osstring_conversion() {
        let half = OsStr::new("\u{bd}"); // ½
        assert_eq!(osstring_to_latin1(half), Some(vec![0xbd]));
        let copy = OsStr::new("\u{a9}"); // ©
        assert_eq!(osstring_to_latin1(copy), Some(vec![0xa9]));
        let heart = OsStr::new("I \u{2764} YOU!"); // ❤
        assert!(osstring_to_latin1(heart).is_none());
        let long = OsStr::new("AVeryLongNameThatDoesNotFitIntoAmigaFileSystem");
        assert!(osstring_to_latin1(long).is_none());
        let empty = OsStr::new("");
        assert!(osstring_to_latin1(empty).is_none());
        let colon = OsStr::new("co:lon");
        assert!(osstring_to_latin1(colon).is_none());
    }
}
