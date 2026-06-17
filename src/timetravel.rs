// SPDX-License-Identifier: GPL-3.0-or-later

//! Reverse-debugging ("rr"-style) substrate: a ring of in-memory machine
//! snapshots plus the bookkeeping to reconstruct any earlier instruction
//! boundary by restoring the nearest snapshot and replaying forward.
//!
//! Copperline's core is already deterministic and already has whole-machine
//! snapshots (`savestate` / `M68kMachine::write_state`), which is the hard
//! part `rr` has to manufacture from nondeterministic real hardware. Reverse
//! debugging here is therefore just: capture periodic snapshots while running
//! forward, and to go back to position P restore the newest snapshot at or
//! before P and single-step forward to P. The replay is byte-identical (see
//! the `save_state_round_trip_replays_identically` guarantee in cpu.rs) as
//! long as the determinism preconditions hold -- see `docs/internals` and the
//! reverse-mode warnings emitted in `emulator.rs`.
//!
//! The engine ops that drive stepping live on `Emulator` (where the step
//! primitives are); this module owns the data structures and budget policy.

use std::collections::VecDeque;

/// One captured machine state. `blob` is the bincode payload produced by
/// `M68kMachine::write_state` (no zlib/magic framing -- snapshots are
/// same-process, same-binary, so the `savestate` versioning is unnecessary
/// and skipping it keeps capture cheap).
pub struct Snapshot {
    /// Monotonic retired-instruction count at capture -- the reverse-debug
    /// position coordinate. Lives outside the serialized state, so capturing
    /// it costs nothing and `STATE_VERSION` is unaffected.
    pub pos: u64,
    /// Emulated frame index at capture.
    pub frame: u64,
    pub blob: Vec<u8>,
}

impl Snapshot {
    fn heap_bytes(&self) -> usize {
        self.blob.len()
    }
}

/// A bounded ring of `Snapshot`s. Oldest entries are evicted once the total
/// blob size exceeds `budget_bytes`; snapshots are taken at most every
/// `interval_frames` emulated frames.
pub struct SnapshotRing {
    snaps: VecDeque<Snapshot>,
    budget_bytes: usize,
    interval_frames: u64,
    used_bytes: usize,
    last_capture_frame: Option<u64>,
}

impl SnapshotRing {
    /// `budget_mb` caps total snapshot memory; `interval_frames` is the
    /// minimum gap between captures (clamped to >= 1). A larger interval
    /// trades reverse-step latency (longer replay between snapshots) for
    /// lower memory and forward-run overhead.
    pub fn new(budget_mb: usize, interval_frames: u64) -> Self {
        Self {
            snaps: VecDeque::new(),
            budget_bytes: budget_mb.saturating_mul(1024 * 1024),
            interval_frames: interval_frames.max(1),
            used_bytes: 0,
            last_capture_frame: None,
        }
    }

    /// Whether a snapshot is due at `frame` given the configured interval.
    /// Always true for the very first capture so the ring has a floor.
    pub fn capture_due(&self, frame: u64) -> bool {
        match self.last_capture_frame {
            None => true,
            Some(last) => frame.saturating_sub(last) >= self.interval_frames,
        }
    }

    /// Insert a snapshot, evicting the oldest entries until the total fits
    /// the byte budget. A single snapshot larger than the whole budget is
    /// still kept (the ring never goes empty after a capture), so reverse
    /// ops always have at least one anchor.
    pub fn push(&mut self, snap: Snapshot) {
        self.last_capture_frame = Some(snap.frame);
        self.used_bytes = self.used_bytes.saturating_add(snap.heap_bytes());
        self.snaps.push_back(snap);
        while self.used_bytes > self.budget_bytes && self.snaps.len() > 1 {
            if let Some(old) = self.snaps.pop_front() {
                self.used_bytes = self.used_bytes.saturating_sub(old.heap_bytes());
            }
        }
    }

    /// Newest snapshot with `pos <= target`, i.e. the closest anchor to
    /// replay forward from. `None` if `target` predates all retained history.
    pub fn nearest_at_or_before(&self, target: u64) -> Option<&Snapshot> {
        self.snaps.iter().rev().find(|s| s.pos <= target)
    }

    /// Newest snapshot at strictly less than `pos` (the anchor for walking
    /// one interval further back than `pos`).
    pub fn nearest_before(&self, pos: u64) -> Option<&Snapshot> {
        self.snaps.iter().rev().find(|s| s.pos < pos)
    }

    /// Position of the oldest retained snapshot -- the earliest point reverse
    /// debugging can still reconstruct. A request before this is "beyond
    /// recorded history".
    pub fn oldest_pos(&self) -> Option<u64> {
        self.snaps.front().map(|s| s.pos)
    }

    pub fn len(&self) -> usize {
        self.snaps.len()
    }

    pub fn is_empty(&self) -> bool {
        self.snaps.is_empty()
    }

    pub fn used_bytes(&self) -> usize {
        self.used_bytes
    }
}

/// A reconstructed memory write reported by the reverse watchpoint: the last
/// instruction that changed a watched location before the target point.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WriteRecord {
    /// Address of the changed word.
    pub addr: u32,
    pub old: u16,
    pub new: u16,
    /// PC of the instruction credited with the change (the same attribution
    /// the forward `COPPERLINE_DBG_WATCH` uses: the instruction the diff is
    /// observed across; a Copper/blitter write lands on the next CPU
    /// instruction's PC).
    pub pc: u32,
    /// Retired-instruction position of the writing instruction.
    pub pos: u64,
    pub cck: u64,
    pub frame: u64,
}

/// Outcome of a reverse-history query that may run off the end of retained
/// snapshots.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReverseOutcome<T> {
    /// The query was answered within retained history.
    Found(T),
    /// No matching event was found, but the whole searched range was covered
    /// by retained snapshots (a definitive "never happened").
    NotFound,
    /// The target predates the oldest retained snapshot; the answer may exist
    /// but is beyond what the ring can reconstruct.
    BeyondHistory,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap(pos: u64, frame: u64, bytes: usize) -> Snapshot {
        Snapshot {
            pos,
            frame,
            blob: vec![0u8; bytes],
        }
    }

    fn newest_pos(ring: &SnapshotRing) -> Option<u64> {
        ring.nearest_at_or_before(u64::MAX).map(|s| s.pos)
    }

    #[test]
    fn capture_due_respects_interval() {
        let ring = SnapshotRing::new(64, 5);
        assert!(ring.capture_due(0), "first capture is always due");
        let mut ring = ring;
        ring.push(snap(0, 10, 8));
        assert!(!ring.capture_due(12), "frame 12 is < 5 frames after 10");
        assert!(!ring.capture_due(14));
        assert!(ring.capture_due(15), "frame 15 is exactly 5 after 10");
        assert!(ring.capture_due(100));
    }

    #[test]
    fn push_evicts_oldest_over_budget() {
        // 1 MiB budget, 400 KiB snapshots -> only two fit at a time.
        let mut ring = SnapshotRing::new(1, 1);
        ring.push(snap(0, 0, 400 * 1024));
        ring.push(snap(1, 1, 400 * 1024));
        assert_eq!(ring.len(), 2);
        ring.push(snap(2, 2, 400 * 1024));
        assert_eq!(ring.len(), 2, "oldest evicted to stay within budget");
        assert_eq!(ring.oldest_pos(), Some(1));
        assert_eq!(newest_pos(&ring), Some(2));
    }

    #[test]
    fn oversized_single_snapshot_is_retained() {
        let mut ring = SnapshotRing::new(1, 1); // 1 MiB budget
        ring.push(snap(0, 0, 4 * 1024 * 1024)); // 4 MiB snapshot
        assert_eq!(ring.len(), 1, "never evict below one anchor");
        assert_eq!(ring.oldest_pos(), Some(0));
    }

    #[test]
    fn nearest_lookups_pick_the_right_anchor() {
        let mut ring = SnapshotRing::new(64, 1);
        ring.push(snap(0, 0, 8));
        ring.push(snap(100, 1, 8));
        ring.push(snap(200, 2, 8));
        assert_eq!(ring.nearest_at_or_before(150).map(|s| s.pos), Some(100));
        assert_eq!(ring.nearest_at_or_before(200).map(|s| s.pos), Some(200));
        assert_eq!(ring.nearest_at_or_before(50).map(|s| s.pos), Some(0));
        assert_eq!(ring.nearest_before(200).map(|s| s.pos), Some(100));
        assert!(ring.nearest_before(0).is_none());
    }
}
