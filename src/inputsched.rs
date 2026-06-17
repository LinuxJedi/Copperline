// SPDX-License-Identifier: GPL-3.0-or-later

//! Deterministic input replay for reverse debugging.
//!
//! Byte-identical reverse replay requires every machine-visible input to be
//! reproduced at exactly the instruction position it was first applied. The
//! live forward run (window.rs) keeps firing scripted and interactive input
//! exactly as it always has; when reverse mode is armed it additionally
//! *notes* each applied action into a [`ReplayInputLog`] keyed by retired
//! instruction position. When a reverse op restores an earlier snapshot and
//! replays forward, the engine re-applies the logged actions as it reaches
//! their positions, so the reconstructed timeline matches the original.
//!
//! Input is recorded at the central bus-affecting helpers (keyboard, mouse
//! buttons, mouse motion, scripted joystick), through which both scripted
//! (`--script` / `--press-after` / ...) and the live window paths funnel, so
//! there is a single record site per kind and no double counting.

use crate::bus::Bus;

/// Full port-2 joystick / CD32-pad held state. Reverse replay re-applies the
/// whole state (matching how the live joystick path asserts the held set),
/// so it is self-contained and order-independent.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct JoyState {
    pub up: bool,
    pub down: bool,
    pub left: bool,
    pub right: bool,
    pub red: bool,
    pub blue: bool,
    pub play: bool,
    pub rwd: bool,
    pub ffw: bool,
    pub green: bool,
    pub yellow: bool,
}

/// One machine-visible input action, captured for deterministic replay.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReplayAction {
    /// Keyboard transition (`rawkey` is the Amiga raw keycode).
    Key { rawkey: u8, pressed: bool },
    /// Port-1 mouse button: index 0 = left, 1 = right, 2 = middle.
    MouseButton { index: u8, pressed: bool },
    /// Port-1 quadrature mouse motion.
    MouseMove { dx: i32, dy: i32 },
    /// Port-2 joystick / CD32-pad held state.
    Joy(JoyState),
    /// A floppy media change occurred. Replaying across a media change cannot
    /// be reconstructed from the log (the inserted image is host-file state),
    /// so the engine warns rather than silently diverging.
    DiskChange,
}

impl ReplayAction {
    /// Re-apply this action to `bus` during reverse replay, using the same
    /// bus-level primitives the live paths use.
    pub fn apply(self, bus: &mut Bus) {
        match self {
            ReplayAction::Key { rawkey, pressed } => {
                if pressed {
                    bus.enqueue_key(rawkey);
                } else {
                    bus.enqueue_key_event(rawkey, false);
                }
            }
            ReplayAction::MouseButton { index, pressed } => match index {
                0 => bus.input.lmb_port1 = pressed,
                1 => bus.input.rmb_port1 = pressed,
                2 => bus.input.mmb_port1 = pressed,
                _ => {}
            },
            ReplayAction::MouseMove { dx, dy } => bus.input.add_mouse_delta_port1(dx, dy),
            ReplayAction::Joy(j) => {
                bus.input
                    .set_joystick_port2(j.up, j.down, j.left, j.right, j.red, j.blue);
                bus.input
                    .set_cd32_buttons_port2(j.play, j.rwd, j.ffw, j.green, j.yellow);
            }
            ReplayAction::DiskChange => {
                log::warn!(
                    "reverse-debug replay crossed a floppy media change; \
                     reconstruction past it may diverge"
                );
            }
        }
    }
}

/// A position-ordered log of applied input actions, plus a replay cursor.
///
/// Entries are appended during the forward run in non-decreasing position
/// order (retired-instruction count is monotonic), so the backing vector is
/// always sorted by position -- `begin_replay` can binary-search it.
#[derive(Default)]
pub struct ReplayInputLog {
    events: Vec<(u64, ReplayAction)>,
    cursor: usize,
}

impl ReplayInputLog {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record an action applied at retired-instruction position `pos`.
    pub fn record(&mut self, pos: u64, action: ReplayAction) {
        self.events.push((pos, action));
    }

    /// Drop entries before `pos` (older than the oldest retained snapshot, so
    /// they can never be replayed again).
    pub fn prune_before(&mut self, pos: u64) {
        let keep_from = self.events.partition_point(|(p, _)| *p < pos);
        if keep_from > 0 {
            self.events.drain(0..keep_from);
        }
    }

    /// Position the replay cursor at the first action at or after `from_pos`,
    /// ready for a replay that starts at `from_pos`.
    pub fn begin_replay(&mut self, from_pos: u64) {
        self.cursor = self.events.partition_point(|(p, _)| *p < from_pos);
    }

    /// Move every action now due (position <= `pos`) into `out`, advancing the
    /// cursor. Actions at the same position come out in record order.
    pub fn take_due(&mut self, pos: u64, out: &mut Vec<ReplayAction>) {
        while self.cursor < self.events.len() && self.events[self.cursor].0 <= pos {
            out.push(self.events[self.cursor].1);
            self.cursor += 1;
        }
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.events.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(rawkey: u8) -> ReplayAction {
        ReplayAction::Key {
            rawkey,
            pressed: true,
        }
    }

    #[test]
    fn take_due_yields_actions_up_to_position_in_order() {
        let mut log = ReplayInputLog::new();
        log.record(10, key(1));
        log.record(10, key(2)); // same position, later record
        log.record(25, key(3));
        log.begin_replay(0);

        let mut out = Vec::new();
        log.take_due(9, &mut out);
        assert!(out.is_empty(), "nothing due before position 10");
        log.take_due(10, &mut out);
        assert_eq!(out, vec![key(1), key(2)], "both at pos 10, in record order");
        out.clear();
        log.take_due(24, &mut out);
        assert!(out.is_empty());
        log.take_due(100, &mut out);
        assert_eq!(out, vec![key(3)]);
    }

    #[test]
    fn begin_replay_skips_actions_before_the_anchor() {
        let mut log = ReplayInputLog::new();
        log.record(5, key(1));
        log.record(15, key(2));
        log.record(25, key(3));
        // Replay starting at position 15 must not re-apply the pos-5 action
        // (it is already baked into the anchor snapshot).
        log.begin_replay(15);
        let mut out = Vec::new();
        log.take_due(1000, &mut out);
        assert_eq!(out, vec![key(2), key(3)]);
    }

    #[test]
    fn prune_before_drops_old_entries() {
        let mut log = ReplayInputLog::new();
        log.record(5, key(1));
        log.record(15, key(2));
        log.record(25, key(3));
        log.prune_before(15);
        assert_eq!(log.len(), 2);
        log.begin_replay(0);
        let mut out = Vec::new();
        log.take_due(1000, &mut out);
        assert_eq!(out, vec![key(2), key(3)]);
    }
}
