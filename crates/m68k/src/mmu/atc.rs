//! Address Translation Cache (ATC).
//!
//! The page-table walk costs several descriptor fetches per access, which is far
//! too expensive to pay on every cycle-stepped memory reference. Real 68030/68040
//! parts cache recent translations in an ATC; we model the same: a small
//! direct-mapped cache of (logical page -> physical page) keyed by the logical
//! page frame and the supervisor flag (user and supervisor can map a page
//! differently via separate root pointers).
//!
//! The ATC is a pure cache: it holds nothing that backing memory does not, so it
//! is never serialized (a save state restores it empty) and is flushed whenever
//! the mapping could change -- a write to TC / a root pointer / a TTR, or a
//! PFLUSH/PFLUSHA. Like real hardware, a plain CPU write to a page-table entry
//! does NOT auto-flush; software must PFLUSH, which is where we flush.

const ATC_ENTRIES: usize = 64;

#[derive(Debug, Clone, Copy, Default)]
struct AtcEntry {
    valid: bool,
    /// `(page_frame << 1) | supervisor`, disambiguating user vs supervisor maps.
    tag: u32,
    /// Physical page base (aligned to the page size in force at fill time).
    phys_page: u32,
    /// Write-protected page (W): a write must fault, not hit.
    write_protected: bool,
    /// Supervisor-only page (S): a user access must fault, not hit.
    supervisor_only: bool,
}

/// A direct-mapped address translation cache.
#[derive(Debug, Clone)]
pub struct Atc {
    entries: [AtcEntry; ATC_ENTRIES],
}

impl Default for Atc {
    fn default() -> Self {
        Self {
            entries: [AtcEntry::default(); ATC_ENTRIES],
        }
    }
}

impl Atc {
    #[inline]
    fn tag(page_frame: u32, supervisor: bool) -> u32 {
        (page_frame << 1) | supervisor as u32
    }

    #[inline]
    fn index(page_frame: u32) -> usize {
        (page_frame as usize) & (ATC_ENTRIES - 1)
    }

    /// Look up the physical page base for `page_frame` (logical address >> page
    /// bits) for this access, or `None` on a miss. A cached entry whose
    /// permissions the access would violate (a write to a write-protected page,
    /// or a user access to a supervisor page) returns `None` so the caller
    /// re-walks and raises the fault -- the permission check is never bypassed.
    #[inline]
    pub fn lookup(&self, page_frame: u32, supervisor: bool, write: bool) -> Option<u32> {
        let e = &self.entries[Self::index(page_frame)];
        if !e.valid || e.tag != Self::tag(page_frame, supervisor) {
            return None;
        }
        if (write && e.write_protected) || (!supervisor && e.supervisor_only) {
            return None;
        }
        Some(e.phys_page)
    }

    /// Record a freshly-walked translation and its protection bits.
    #[inline]
    pub fn insert(
        &mut self,
        page_frame: u32,
        supervisor: bool,
        phys_page: u32,
        write_protected: bool,
        supervisor_only: bool,
    ) {
        self.entries[Self::index(page_frame)] = AtcEntry {
            valid: true,
            tag: Self::tag(page_frame, supervisor),
            phys_page,
            write_protected,
            supervisor_only,
        };
    }

    /// Invalidate everything (PFLUSHA, TC/root-pointer/TTR write, reset).
    pub fn flush_all(&mut self) {
        for e in &mut self.entries {
            e.valid = false;
        }
    }

    /// Invalidate the entry for one logical page frame (PFLUSH `(An)`), both the
    /// user and supervisor variant since the same slot holds either.
    pub fn flush_page(&mut self, page_frame: u32) {
        let e = &mut self.entries[Self::index(page_frame)];
        if e.tag >> 1 == page_frame {
            e.valid = false;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hit_after_insert_miss_after_flush() {
        let mut atc = Atc::default();
        assert_eq!(atc.lookup(0x10, false, false), None);
        atc.insert(0x10, false, 0x8000, false, false);
        assert_eq!(atc.lookup(0x10, false, false), Some(0x8000));
        // Supervisor map of the same page frame is a distinct entry.
        assert_eq!(atc.lookup(0x10, true, false), None);
        atc.flush_all();
        assert_eq!(atc.lookup(0x10, false, false), None);
    }

    #[test]
    fn flush_page_drops_only_that_frame() {
        let mut atc = Atc::default();
        atc.insert(0x10, false, 0x1000, false, false);
        atc.flush_page(0x11); // different frame, same is unlikely-index: no-op for 0x10
        assert_eq!(atc.lookup(0x10, false, false), Some(0x1000));
        atc.flush_page(0x10);
        assert_eq!(atc.lookup(0x10, false, false), None);
    }

    #[test]
    fn permission_violation_does_not_hit() {
        let mut atc = Atc::default();
        // Write-protected, supervisor-only page.
        atc.insert(0x10, true, 0x5000, true, true);
        // A supervisor read hits; a write misses (write-protected) so the caller
        // re-walks and faults.
        assert_eq!(atc.lookup(0x10, true, false), Some(0x5000));
        assert_eq!(atc.lookup(0x10, true, true), None);
    }
}
