// SPDX-License-Identifier: GPL-3.0-or-later

//! 68020/68030/68040 on-chip cache model (functional + bus-traffic accurate).
//!
//! Each cache is a power-of-two number of direct-mapped longword entries. The
//! 68020/68030 use 64 entries (256 bytes), the exact 68020 instruction-cache
//! geometry; the 68030's 16x4-longword line organisation collapses to the same
//! thing once burst fills are ignored. The 68040's 4 KB caches are 1024
//! entries: the larger capacity is what matters here (a chip-RAM loop bigger
//! than 256 bytes stays resident on a 040 where it would thrash a 020). The
//! 040's 4-way set-associative, 16-byte-line, copyback organisation is not
//! modelled, and need not be: copyback is invisible because the data cache only
//! covers expansion RAM, which is not DMA-visible, so write-back versus
//! write-through is unobservable. We do not model burst timing (IBE/DBE are
//! stored but inert on the 030).
//!
//! A hit serves the access with no bus cycle, which is the real effect that
//! matters on an Amiga: cached fetches stop competing with DMA for the chip
//! bus. Like the real silicon, the instruction cache does NOT snoop writes -
//! self-modifying code must clear it (through CACR on the 020/030, through
//! CINV/CPUSH on the 040), and DMA (blitter/copper/disk) never invalidates it.
//! The data cache is write-through; a write that hits invalidates the entry
//! (the next read refills it), which is always coherent and only marginally
//! pessimistic versus update-on-hit.
//!
//! The caches are modelled by default on the CPUs that have them (AmigaOS
//! turns them on through CACR at boot); `[cpu] icache = false` / `dcache =
//! false` force them off. The 020+ chip-bus timing is calibrated against an
//! A1200 with the cache on, since that is the A1200 default.

/// Longword entries for the 256-byte 68020/68030 caches.
pub const LINES_020: usize = 64;
/// Longword entries for the 4 KB 68040 caches.
pub const LINES_040: usize = 1024;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct LongwordCache {
    tags: Vec<u32>,
    valid: Vec<bool>,
    data: Vec<u32>,
    /// `lines - 1`; `lines` is a power of two. `index = (addr >> 2) & index_mask`.
    index_mask: u32,
    /// Right-shift that drops the byte-offset and index bits, leaving the tag.
    tag_shift: u32,
}

impl Default for LongwordCache {
    fn default() -> Self {
        Self::new(LINES_020)
    }
}

impl LongwordCache {
    /// `lines` must be a power of two.
    pub fn new(lines: usize) -> Self {
        debug_assert!(lines.is_power_of_two(), "cache line count must be 2^n");
        Self {
            tags: vec![0; lines],
            valid: vec![false; lines],
            data: vec![0; lines],
            index_mask: (lines as u32) - 1,
            // Two byte-offset bits below the index, then log2(lines) index bits.
            tag_shift: 2 + lines.trailing_zeros(),
        }
    }

    #[inline]
    fn index(&self, addr: u32) -> usize {
        ((addr >> 2) & self.index_mask) as usize
    }

    #[inline]
    fn tag(&self, addr: u32) -> u32 {
        addr >> self.tag_shift
    }

    /// Look up the aligned longword containing `addr`.
    #[inline]
    pub fn lookup(&self, addr: u32) -> Option<u32> {
        let idx = self.index(addr);
        if self.valid[idx] && self.tags[idx] == self.tag(addr) {
            Some(self.data[idx])
        } else {
            None
        }
    }

    /// Fill the entry for the aligned longword containing `addr`.
    #[inline]
    pub fn fill(&mut self, addr: u32, longword: u32) {
        let idx = self.index(addr);
        self.tags[idx] = self.tag(addr);
        self.valid[idx] = true;
        self.data[idx] = longword;
    }

    /// Invalidate the entry holding `addr`, if any (CACR clear-entry, and
    /// data-cache write-through hits).
    #[inline]
    pub fn invalidate_entry(&mut self, addr: u32) {
        let idx = self.index(addr);
        if self.tags[idx] == self.tag(addr) {
            self.valid[idx] = false;
        }
    }

    /// Invalidate everything (CACR clear-cache, CINV/CPUSH, reset).
    pub fn invalidate_all(&mut self) {
        self.valid.fill(false);
    }
}

/// One cache (instruction or data) plus its CACR-controlled state.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CpuCache {
    cache: LongwordCache,
    /// CACR enable bit (EI/ED on 020/030, IE/DE on 040).
    pub enabled: bool,
    /// CACR freeze bit (FI/FD): hits are served, misses do not allocate.
    /// Always false on the 040, which has no freeze bit.
    pub frozen: bool,
}

impl Default for CpuCache {
    fn default() -> Self {
        Self::new(LINES_020)
    }
}

impl CpuCache {
    /// A disabled cache with `lines` longword entries (a power of two):
    /// `LINES_020` for the 020/030, `LINES_040` for the 040.
    pub fn new(lines: usize) -> Self {
        Self {
            cache: LongwordCache::new(lines),
            enabled: false,
            frozen: false,
        }
    }

    /// Serve a `size`-byte read at `addr` from the cache, if every
    /// longword it touches is resident. `size` must be 1, 2, or 4.
    pub fn read(&self, addr: u32, size: usize) -> Option<u32> {
        if !self.enabled {
            return None;
        }
        let first = self.cache.lookup(addr)?;
        let off = (addr & 3) as usize;
        if off + size <= 4 {
            let shift = (4 - off - size) * 8;
            let mask = if size == 4 {
                0xFFFF_FFFF
            } else {
                (1u32 << (size * 8)) - 1
            };
            return Some((first >> shift) & mask);
        }
        // The access straddles two longwords (misaligned 68020+ access).
        let second = self.cache.lookup(addr.wrapping_add(4) & !3)?;
        let head = off + size - 4; // bytes from the second longword
        let value = ((u64::from(first) << 32) | u64::from(second)) >> ((4 - head) * 8);
        Some(
            (value as u32)
                & if size == 4 {
                    0xFFFF_FFFF
                } else {
                    (1u32 << (size * 8)) - 1
                },
        )
    }

    /// Allocate the longword(s) covering a missed `size`-byte access at
    /// `addr`, honouring the freeze bit. `fetch` peeks the aligned
    /// longword from backing memory without billing bus time (the miss
    /// itself was already billed by the normal access path).
    pub fn fill_after_miss(&mut self, addr: u32, size: usize, mut fetch: impl FnMut(u32) -> u32) {
        if !self.enabled || self.frozen {
            return;
        }
        let first = addr & !3;
        let last = addr.wrapping_add(size as u32 - 1) & !3;
        let mut long = first;
        loop {
            if self.cache.lookup(long).is_none() {
                let value = fetch(long);
                self.cache.fill(long, value);
            }
            if long == last {
                break;
            }
            long = long.wrapping_add(4);
        }
    }

    /// A write went past the cache (write-through): drop any entry it
    /// touches so later reads refill from memory.
    pub fn invalidate_write(&mut self, addr: u32, size: usize) {
        let first = addr & !3;
        let last = addr.wrapping_add(size.max(1) as u32 - 1) & !3;
        self.cache.invalidate_entry(first);
        if last != first {
            self.cache.invalidate_entry(last);
        }
    }

    /// CACR clear-entry strobe: drop the line indexed by CAAR (020/030 only;
    /// the 040 has no per-entry CACR strobe).
    pub fn clear_entry_by_index(&mut self, caar: u32) {
        let idx = self.cache.index(caar);
        self.cache.valid[idx] = false;
    }

    pub fn clear_all(&mut self) {
        self.cache.invalidate_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn enabled_cache() -> CpuCache {
        CpuCache {
            enabled: true,
            ..CpuCache::default()
        }
    }

    #[test]
    fn miss_then_fill_then_hit() {
        let mut c = enabled_cache();
        assert_eq!(c.read(0x1000, 2), None);
        c.fill_after_miss(0x1000, 2, |addr| {
            assert_eq!(addr, 0x1000);
            0x1234_5678
        });
        assert_eq!(c.read(0x1000, 2), Some(0x1234));
        assert_eq!(c.read(0x1002, 2), Some(0x5678));
        assert_eq!(c.read(0x1000, 4), Some(0x1234_5678));
        assert_eq!(c.read(0x1001, 1), Some(0x34));
    }

    #[test]
    fn straddling_read_needs_both_longwords() {
        let mut c = enabled_cache();
        c.fill_after_miss(0x1000, 4, |_| 0x1122_3344);
        assert_eq!(c.read(0x1002, 4), None);
        c.fill_after_miss(0x1004, 4, |_| 0x5566_7788);
        assert_eq!(c.read(0x1002, 4), Some(0x3344_5566));
        assert_eq!(c.read(0x1003, 2), Some(0x4455));
    }

    #[test]
    fn disabled_cache_serves_nothing_and_frozen_does_not_allocate() {
        let mut c = CpuCache::default();
        c.fill_after_miss(0x1000, 4, |_| 0xAAAA_AAAA);
        c.enabled = true;
        assert_eq!(c.read(0x1000, 4), None);

        c.frozen = true;
        c.fill_after_miss(0x1000, 4, |_| 0xAAAA_AAAA);
        assert_eq!(c.read(0x1000, 4), None);

        // Unfrozen it allocates; refrozen it still serves hits.
        c.frozen = false;
        c.fill_after_miss(0x1000, 4, |_| 0xAAAA_AAAA);
        c.frozen = true;
        assert_eq!(c.read(0x1000, 4), Some(0xAAAA_AAAA));
    }

    #[test]
    fn same_index_different_tag_evicts() {
        let mut c = enabled_cache();
        c.fill_after_miss(0x1000, 4, |_| 1);
        // 0x1100 maps to the same line index (bits 7..2 equal), new tag.
        c.fill_after_miss(0x1100, 4, |_| 2);
        assert_eq!(c.read(0x1100, 4), Some(2));
        assert_eq!(c.read(0x1000, 4), None);
    }

    #[test]
    fn larger_cache_avoids_a_conflict_the_small_one_evicts() {
        // 0x1000 and 0x1100 land on the same line index in the 256-byte
        // (64-line) 020/030 cache - bits [2..8) match - so the second fill
        // evicts the first. In the 4 KB (1024-line) 040 cache the index spans
        // bits [2..12), so they map to different lines and both stay resident.
        // This is the capacity difference that keeps a >256-byte chip-RAM loop
        // cached on a 040 where it would thrash a 020.
        let mut small = CpuCache::new(LINES_020);
        small.enabled = true;
        small.fill_after_miss(0x1000, 4, |_| 1);
        small.fill_after_miss(0x1100, 4, |_| 2);
        assert_eq!(small.read(0x1100, 4), Some(2));
        assert_eq!(
            small.read(0x1000, 4),
            None,
            "small cache evicts on conflict"
        );

        let mut big = CpuCache::new(LINES_040);
        big.enabled = true;
        big.fill_after_miss(0x1000, 4, |_| 1);
        big.fill_after_miss(0x1100, 4, |_| 2);
        assert_eq!(big.read(0x1000, 4), Some(1), "large cache keeps both");
        assert_eq!(big.read(0x1100, 4), Some(2));
    }

    #[test]
    fn write_invalidates_touched_longwords() {
        let mut c = enabled_cache();
        c.fill_after_miss(0x1000, 4, |_| 1);
        c.fill_after_miss(0x1004, 4, |_| 2);
        c.invalidate_write(0x1002, 4); // straddles both
        assert_eq!(c.read(0x1000, 4), None);
        assert_eq!(c.read(0x1004, 4), None);
    }

    #[test]
    fn clear_entry_by_caar_index_drops_only_that_line() {
        let mut c = enabled_cache();
        c.fill_after_miss(0x1000, 4, |_| 1);
        c.fill_after_miss(0x1004, 4, |_| 2);
        c.clear_entry_by_index(0x1000);
        assert_eq!(c.read(0x1000, 4), None);
        assert_eq!(c.read(0x1004, 4), Some(2));
    }
}
