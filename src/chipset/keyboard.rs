// SPDX-License-Identifier: GPL-3.0-or-later

//! Amiga keyboard MCU (6500/1) model: bit-timed KCLK/KDAT serial path
//! into CIA-A.
//!
//! A real Amiga keyboard is a microcontroller that scans the key matrix
//! and clocks each event to the machine over two open-drain lines: KCLK
//! (wired to CIA-A CNT) and KDAT (wired to CIA-A SP). The CIA shifts the
//! KDAT level into SDR on every KCLK rising edge and raises the SP
//! interrupt after eight bits. This module models that MCU as a state
//! machine on the emulated clock; nothing is injected into SDR directly.
//!
//! Protocol reference: the HRM keyboard appendix, cross-checked against
//! real-hardware-validated replacement keyboard-controller firmware
//! (A500KBFirmware: send_data/sync_with_computer/run_reset/main_loop in
//! common/ drive actual Amigas through this exact protocol).
//!
//! - Every byte (key events and status codes alike) is rotated left one
//!   position and sent MSB-first with logical 1 driving KDAT low. The
//!   CIA shifts the raw KDAT pin level into SDR, so SDR ends up holding
//!   the inverted rotated value -- which software decodes with `ror.b` +
//!   `not.b`. For a key event the logical value is
//!   `(up_down << 7) | keycode`, so SDR holds
//!   `~((keycode << 1) | up_down)`.
//! - Each bit cell is three 20 us phases: KDAT asserted (data setup),
//!   KCLK low, KCLK high. The CIA samples KDAT on the KCLK low->high
//!   edge. A byte takes ~480 us; KCLK idles high.
//! - After the 8th bit the keyboard releases KDAT and waits for the
//!   handshake: the Amiga drives KDAT low (CIA-A SPMODE output mode).
//!   The 6500/1 polls slowly, so the pulse must last at least 85 us;
//!   shorter pulses are ignored (the replacement firmware polls faster
//!   and accepts shorter ones -- we model the original part).
//! - No handshake within 143 ms -> lost sync: the keyboard clocks out a
//!   single logical-1 bit (KDAT low) and waits another 143 ms, repeating
//!   until a bit is handshaked; it then sends $F9 ("last key code bad")
//!   followed by a retransmission of the lost event (HRM; the
//!   replacement firmware sends $F9 without the retransmission).
//! - Power-up / post-reset: after self-test the keyboard synchronizes
//!   with the same lone-bit procedure, then sends $FD ("initiate
//!   power-up key stream"), the codes of keys already held, and $FE
//!   ("terminate key stream"). Power-up bytes do not retry on a missed
//!   handshake (matching the firmware's ignored send results).
//! - Ctrl + both Amiga keys: the keyboard sends $78 (reset warning),
//!   and if it is handshaked, a second $78 with up to 250 ms for the
//!   processor to acknowledge; it then pulls KCLK low for >= 500 ms,
//!   hard-resetting the machine, and restarts its own power-up flow.

use std::collections::VecDeque;

use crate::chipset::cia::Cia;
use crate::chipset::paula::PAULA_CLOCK_HZ;

const fn us_to_cck(us: u64) -> u64 {
    PAULA_CLOCK_HZ as u64 * us / 1_000_000
}

/// Each of the three bit-cell phases (KDAT setup, KCLK low, KCLK high)
/// is 20 us (~70 cck); one bit is 60 us, a byte ~480 us.
const BIT_PHASE_CCK: u64 = us_to_cck(20);
/// Minimum Amiga-side KDAT-low pulse the MCU accepts as a handshake.
pub(crate) const HANDSHAKE_MIN_CCK: u64 = us_to_cck(85);
/// No handshake for this long after a byte -> lost-sync recovery.
pub(crate) const RESYNC_TIMEOUT_CCK: u64 = us_to_cck(143_000);
/// MCU firmware latency between an accepted handshake and the first
/// KCLK edge of the next byte (matrix scan loop, not a documented
/// constant; small against the 480 us byte time).
const INTER_BYTE_GAP_CCK: u64 = us_to_cck(100);
/// Window for the processor to acknowledge the second reset warning.
const RESET_ACK_TIMEOUT_CCK: u64 = us_to_cck(250_000);
/// KCLK held low this long forces the system reset.
pub(crate) const KCLK_RESET_HOLD_CCK: u64 = us_to_cck(500_000);
/// MCU self-test/start-up time before it first tries to synchronize.
/// Not a precisely documented figure; short against boot time.
pub(crate) const SELF_TEST_CCK: u64 = us_to_cck(50_000);

/// "Last key code bad": sent after lost-sync recovery, followed by a
/// retransmission of the lost event.
const STATUS_LAST_CODE_BAD: u8 = 0xF9;
/// "Initiate power-up key stream".
const STATUS_INIT_START: u8 = 0xFD;
/// "Terminate key stream".
const STATUS_INIT_END: u8 = 0xFE;
/// Reset warning, sent (twice) before the keyboard hard-resets the
/// machine via KCLK.
const STATUS_RESET_WARNING: u8 = 0x78;

/// "Output buffer overflow": the type-ahead buffer filled and events
/// were lost.
const STATUS_OVERFLOW: u8 = 0xFA;

const RAWKEY_CTRL: u8 = 0x63;
const RAWKEY_LEFT_AMIGA: u8 = 0x66;
const RAWKEY_RIGHT_AMIGA: u8 = 0x67;
const RAWKEY_CAPS_LOCK: u8 = 0x62;

/// The original 6500/1 keyboard buffers about 10 type-ahead events.
const TYPEAHEAD_CAPACITY: usize = 10;

/// The A500 key matrix, 15 rows x 6 columns of rawkeys (taken from the
/// real keyboard wiring via the A500KBFirmware keymap; the 7 qualifier
/// keys are on dedicated lines and are not part of the matrix, which is
/// why they never ghost -- "qualifier rollover"). Caps Lock IS in the
/// matrix.
#[rustfmt::skip]
const MATRIX: [u8; 90] = [
    0x5F, 0x4C, 0x4F, 0x4E, 0x4D, 0x4A,
    0x59, 0x0D, 0x44, 0x46, 0x41, 0x0F,
    0x58, 0x0C, 0x1B, 0x2B, 0x40, 0x1D,
    0x57, 0x0B, 0x1A, 0x2A, 0x3B, 0x2D,
    0x56, 0x0A, 0x19, 0x29, 0x3A, 0x3D,
    0x5C, 0x09, 0x18, 0x28, 0x39, 0x43,
    0x55, 0x08, 0x17, 0x27, 0x38, 0x1E,
    0x5B, 0x07, 0x16, 0x26, 0x37, 0x2E,
    0x54, 0x06, 0x15, 0x25, 0x36, 0x3E,
    0x53, 0x05, 0x14, 0x24, 0x35, 0x3C,
    0x52, 0x04, 0x13, 0x23, 0x34, 0x1F,
    0x51, 0x03, 0x12, 0x22, 0x33, 0x2F,
    0x50, 0x02, 0x11, 0x21, 0x32, 0x3F,
    0x5A, 0x01, 0x10, 0x20, 0x31, 0x5E,
    0x45, 0x00, 0x42, 0x62, 0x30, 0x5D,
];

/// (column, row) of a rawkey in the matrix, or None for the qualifier
/// keys and codes the matrix does not carry.
fn matrix_pos(rawkey: u8) -> Option<(usize, usize)> {
    MATRIX
        .iter()
        .position(|&k| k == rawkey)
        .map(|i| (i % 6, i / 6))
}

/// The on-wire form of a logical byte: rotated left one position and
/// inverted (logical 1 drives KDAT low; the CIA shifts the raw pin
/// level). Software undoes this with `ror.b` + `not.b`. Applies to key
/// events and status codes alike -- the MCU firmware rotates every
/// byte it transmits.
const fn on_wire(value: u8) -> u8 {
    !value.rotate_left(1)
}

/// On-wire encoding of one key transition: the logical value is
/// `(up_down << 7) | keycode`, so SDR receives
/// `~((keycode << 1) | up_down)`.
pub(crate) fn encode_keyboard_byte(amiga_raw_keycode: u8, pressed: bool) -> u8 {
    let kc = amiga_raw_keycode & 0x7F;
    let up_down: u8 = if pressed { 0 } else { 1 };
    on_wire((up_down << 7) | kc)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
enum BitPhase {
    /// KDAT asserted for this bit, KCLK still high from the previous
    /// cell; at the end of this phase KCLK falls.
    DataSetup,
    /// KCLK low; at the end of this phase the line rises and the CIA
    /// samples KDAT.
    ClkLow,
    /// KCLK high with data still held; at the end of this phase the
    /// next bit's setup begins (or the byte is complete).
    ClkHigh,
}

/// What a completed lone-bit synchronization leads to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
enum AfterSync {
    /// Lost-byte recovery: send $F9, then retransmit the lost event.
    ResendLost(u8),
    /// Power-up: send $FD, the buffered key stream, then $FE.
    PowerUpStream,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
enum SentKind {
    /// A queued key event.
    Normal,
    /// The $F9 status byte; carries the lost on-wire event byte to
    /// retransmit after the handshake.
    ResyncBad { lost: u8 },
    /// $FD: power-up stream follows.
    PowerUpStart,
    /// A key event inside the power-up stream.
    PowerUpKey,
    /// $FE: power-up stream complete.
    PowerUpEnd,
    /// First $78 reset warning.
    ResetWarnFirst,
    /// Second $78 reset warning (the processor gets 250 ms to ack).
    ResetWarnSecond,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
enum McuState {
    /// MCU start-up self-test delay after power-on or a keyboard reset.
    PowerUpSelfTest { remaining_cck: u64 },
    /// Nothing in flight; next buffered event starts immediately.
    Idle,
    /// Firmware gap between an accepted handshake and the next byte.
    InterByteGap { remaining_cck: u64 },
    /// Shifting the on-wire `byte` to the CIA, bit 0..=7 MSB-first.
    SendingByte {
        byte: u8,
        kind: SentKind,
        bit: u8,
        phase: BitPhase,
        next_edge_in_cck: u64,
    },
    /// Byte complete, KDAT released; waiting for the Amiga's handshake
    /// pulse (measured against the MCU clock, so a pulse that began
    /// during the byte's final bit cell still counts its full width).
    /// `elapsed_cck` counts time in this state for the resync timeout.
    AwaitHandshake {
        byte: u8,
        kind: SentKind,
        elapsed_cck: u64,
    },
    /// Out of sync (or synchronizing at power-up): clocking out a
    /// single logical-1 bit.
    SyncingBit {
        after: AfterSync,
        phase: BitPhase,
        next_edge_in_cck: u64,
    },
    /// Waiting (up to another 143 ms) for the sync bit's handshake.
    SyncAwaitHandshake { after: AfterSync, elapsed_cck: u64 },
    /// KCLK held low; on expiry the system reset fires and the MCU
    /// restarts its own power-up flow.
    HoldingReset { remaining_cck: u64 },
}

/// The keyboard MCU. Owned by `Bus`, ticked from `tick_timed_devices`
/// with colour-clock deltas; all state is plain data (save states).
#[derive(serde::Serialize, serde::Deserialize)]
pub struct KeyboardMcu {
    state: McuState,
    /// On-wire encoded events waiting to transmit (type-ahead buffer).
    /// At power-up this holds the events of keys already pressed,
    /// which the $FD/$FE stream reports.
    buffer: VecDeque<u8>,
    /// Keys currently held, by rawkey bit (the MCU's view of the
    /// matrix); drives the Ctrl+Amiga+Amiga chord detection and
    /// survives a keyboard reset, as the physical keys stay pressed.
    held: [u64; 2],
    /// Monotonic emulated clock, advanced by tick(); KDAT pulse widths
    /// are measured against it so a handshake that starts during the
    /// last bit cell of a byte still counts its full width.
    now_cck: u64,
    /// When the Amiga drove KDAT low (CIA-A SPMODE 0->1), if it still
    /// holds the line.
    kdat_low_since: Option<u64>,
    /// Latched request for a machine reset (KCLK held low long
    /// enough); the Bus copies it into its pending flag.
    system_reset_request: bool,
    /// Caps Lock LED state, owned by the keyboard: the key toggles it,
    /// sending a press code on toggle-on and a release code on
    /// toggle-off; the physical key release sends nothing.
    caps_lock_on: bool,
    /// Events were dropped on a full type-ahead buffer; $FA is owed as
    /// soon as there is room again.
    overflow_pending: bool,
    /// Presses suppressed because they completed an electrically
    /// ambiguous matrix rectangle (ghost/phantom positions); reported
    /// as fresh presses if the ambiguity clears while still held.
    ghost_pending: [u64; 2],
}

impl Default for KeyboardMcu {
    fn default() -> Self {
        Self::new()
    }
}

impl KeyboardMcu {
    pub fn new() -> Self {
        Self {
            state: McuState::PowerUpSelfTest {
                remaining_cck: SELF_TEST_CCK,
            },
            buffer: VecDeque::new(),
            held: [0; 2],
            now_cck: 0,
            kdat_low_since: None,
            system_reset_request: false,
            caps_lock_on: false,
            overflow_pending: false,
            ghost_pending: [0; 2],
        }
    }

    /// The Caps Lock LED, driven by the keyboard MCU itself. Not yet
    /// surfaced in the UI; kept as the accessor a status-bar LED needs.
    #[allow(dead_code)]
    pub fn caps_lock_led(&self) -> bool {
        self.caps_lock_on
    }

    /// Restart the MCU's power-up flow (after a keyboard-driven or
    /// host-driven machine reset). Keys physically held stay held, so
    /// the upcoming power-up stream reports them.
    pub fn begin_power_up(&mut self) {
        self.state = McuState::PowerUpSelfTest {
            remaining_cck: SELF_TEST_CCK,
        };
        self.buffer.clear();
        self.kdat_low_since = None;
        self.system_reset_request = false;
        self.overflow_pending = false;
        self.ghost_pending = [0; 2];
        // Re-buffer press events for keys still held: the power-up
        // stream reports them between $FD and $FE.
        for rawkey in 0..=0x7Fu8 {
            if self.is_held(rawkey) {
                self.buffer.push_back(encode_keyboard_byte(rawkey, true));
            }
        }
    }

    /// The system-reset request latched by a completed KCLK reset hold;
    /// reading clears it.
    pub fn take_system_reset_request(&mut self) -> bool {
        std::mem::take(&mut self.system_reset_request)
    }

    fn is_held(&self, rawkey: u8) -> bool {
        let idx = (rawkey & 0x7F) as usize;
        self.held[idx / 64] & (1 << (idx % 64)) != 0
    }

    fn set_held(&mut self, rawkey: u8, held: bool) {
        let idx = (rawkey & 0x7F) as usize;
        if held {
            self.held[idx / 64] |= 1 << (idx % 64);
        } else {
            self.held[idx / 64] &= !(1 << (idx % 64));
        }
    }

    fn reset_chord_held(&self) -> bool {
        self.is_held(RAWKEY_CTRL)
            && self.is_held(RAWKEY_LEFT_AMIGA)
            && self.is_held(RAWKEY_RIGHT_AMIGA)
    }

    fn in_reset_flow(&self) -> bool {
        matches!(
            self.state,
            McuState::HoldingReset { .. }
                | McuState::SendingByte {
                    kind: SentKind::ResetWarnFirst | SentKind::ResetWarnSecond,
                    ..
                }
                | McuState::AwaitHandshake {
                    kind: SentKind::ResetWarnFirst | SentKind::ResetWarnSecond,
                    ..
                }
        )
    }

    /// Queue a key transition observed by the matrix scan. Detects the
    /// Ctrl+Amiga+Amiga chord (reset-warning protocol), Caps Lock
    /// toggling, type-ahead overflow, and matrix ghost suppression.
    pub fn key_transition(&mut self, rawkey: u8, pressed: bool) {
        if rawkey & 0x7F == RAWKEY_CAPS_LOCK {
            // The keyboard owns the Caps Lock LED: pressing the key
            // toggles it and sends a press code (LED on) or a release
            // code (LED off); the physical release sends nothing.
            if pressed {
                self.caps_lock_on = !self.caps_lock_on;
                let on = self.caps_lock_on;
                self.enqueue_event(RAWKEY_CAPS_LOCK, on);
            }
            return;
        }
        self.set_held(rawkey, pressed);
        if self.in_reset_flow() {
            return;
        }
        if self.reset_chord_held() {
            // The firmware's main loop preempts everything for the
            // reboot path; buffered events are not sent.
            self.buffer.clear();
            self.overflow_pending = false;
            self.state = Self::sending(on_wire(STATUS_RESET_WARNING), SentKind::ResetWarnFirst);
            return;
        }
        if pressed {
            if self.press_is_ghost_ambiguous(rawkey) {
                // The matrix cannot distinguish this press from its
                // phantom corner; the MCU suppresses it.
                self.set_ghost_pending(rawkey, true);
                return;
            }
            self.enqueue_event(rawkey, true);
        } else {
            if self.is_ghost_pending(rawkey) {
                // Never reported; its release is silent too.
                self.set_ghost_pending(rawkey, false);
                return;
            }
            self.enqueue_event(rawkey, false);
            // A release can clear an ambiguity: suppressed keys still
            // held re-appear to the scan as fresh presses.
            self.report_unblocked_ghosts();
        }
    }

    /// Whether pressing `rawkey` completes an electrically ambiguous
    /// rectangle: another held matrix key in its row and another in its
    /// column make the fourth corner read as pressed too, so the scan
    /// cannot tell the real key from the phantom. Qualifiers live on
    /// dedicated lines and never participate.
    fn press_is_ghost_ambiguous(&self, rawkey: u8) -> bool {
        let Some((col, row)) = matrix_pos(rawkey) else {
            return false;
        };
        let row_mate = (0..6).any(|c| c != col && self.is_held(MATRIX[row * 6 + c]));
        let col_mate = (0..15).any(|r| r != row && self.is_held(MATRIX[r * 6 + col]));
        row_mate && col_mate
    }

    fn is_ghost_pending(&self, rawkey: u8) -> bool {
        let idx = (rawkey & 0x7F) as usize;
        self.ghost_pending[idx / 64] & (1 << (idx % 64)) != 0
    }

    fn set_ghost_pending(&mut self, rawkey: u8, pending: bool) {
        let idx = (rawkey & 0x7F) as usize;
        if pending {
            self.ghost_pending[idx / 64] |= 1 << (idx % 64);
        } else {
            self.ghost_pending[idx / 64] &= !(1 << (idx % 64));
        }
    }

    /// Report suppressed presses whose ambiguity has cleared.
    fn report_unblocked_ghosts(&mut self) {
        for rawkey in 0..=0x7Fu8 {
            if self.is_ghost_pending(rawkey)
                && self.is_held(rawkey)
                && !self.press_is_ghost_ambiguous(rawkey)
            {
                self.set_ghost_pending(rawkey, false);
                self.enqueue_event(rawkey, true);
            }
        }
    }

    /// Push one encoded event into the type-ahead buffer, modelling the
    /// 6500/1's ~10-event capacity: events arriving while full are lost
    /// and $FA ("output buffer overflow") is reported as soon as there
    /// is room again.
    fn enqueue_event(&mut self, rawkey: u8, pressed: bool) {
        if self.overflow_pending && self.buffer.len() < TYPEAHEAD_CAPACITY {
            self.buffer.push_back(on_wire(STATUS_OVERFLOW));
            self.overflow_pending = false;
        }
        if self.buffer.len() >= TYPEAHEAD_CAPACITY {
            self.overflow_pending = true;
            return;
        }
        self.buffer.push_back(encode_keyboard_byte(rawkey, pressed));
    }

    /// The state following an accepted handshake for `kind`.
    fn after_handshake(&mut self, kind: SentKind) -> McuState {
        match kind {
            SentKind::Normal => McuState::InterByteGap {
                remaining_cck: INTER_BYTE_GAP_CCK,
            },
            // $F9 acknowledged: retransmit the lost event.
            SentKind::ResyncBad { lost } => Self::sending(lost, SentKind::Normal),
            SentKind::PowerUpStart | SentKind::PowerUpKey => self.next_power_up_byte(),
            SentKind::PowerUpEnd => McuState::InterByteGap {
                remaining_cck: INTER_BYTE_GAP_CCK,
            },
            SentKind::ResetWarnFirst => {
                Self::sending(on_wire(STATUS_RESET_WARNING), SentKind::ResetWarnSecond)
            }
            SentKind::ResetWarnSecond => McuState::HoldingReset {
                remaining_cck: KCLK_RESET_HOLD_CCK,
            },
        }
    }

    /// The state after `kind`'s handshake window expired.
    fn after_handshake_timeout(&mut self, kind: SentKind, byte: u8) -> McuState {
        match kind {
            // A lost key event: recover through the sync procedure.
            SentKind::Normal => McuState::SyncingBit {
                after: AfterSync::ResendLost(byte),
                phase: BitPhase::DataSetup,
                next_edge_in_cck: BIT_PHASE_CCK,
            },
            // The $F9 itself went unanswered: resynchronize again,
            // still owing the lost event.
            SentKind::ResyncBad { lost } => McuState::SyncingBit {
                after: AfterSync::ResendLost(lost),
                phase: BitPhase::DataSetup,
                next_edge_in_cck: BIT_PHASE_CCK,
            },
            // Power-up bytes do not retry (firmware ignores the send
            // results); march on through the stream.
            SentKind::PowerUpStart | SentKind::PowerUpKey => self.next_power_up_byte(),
            SentKind::PowerUpEnd => McuState::Idle,
            // No one listened to the warning: reset anyway.
            SentKind::ResetWarnFirst | SentKind::ResetWarnSecond => McuState::HoldingReset {
                remaining_cck: KCLK_RESET_HOLD_CCK,
            },
        }
    }

    /// Next byte of the power-up stream: a buffered held-key event, or
    /// $FE when the stream is done.
    fn next_power_up_byte(&mut self) -> McuState {
        match self.buffer.pop_front() {
            Some(byte) => Self::sending(byte, SentKind::PowerUpKey),
            None => Self::sending(on_wire(STATUS_INIT_END), SentKind::PowerUpEnd),
        }
    }

    /// The Amiga drove (true) or released (false) KDAT via CIA-A SPMODE.
    /// The pulse is timed on the MCU clock from the actual drive edge,
    /// so a handshake that begins while the byte's final bit cell is
    /// still clocking out is credited with its full width -- the boot ROM
    /// pulse is barely over the 85 us minimum, and the 6500/1 samples
    /// the line level, not the protocol state.
    pub fn amiga_kdat_edge(&mut self, driven_low: bool) {
        if driven_low {
            self.kdat_low_since = Some(self.now_cck);
            return;
        }
        let Some(since) = self.kdat_low_since.take() else {
            return;
        };
        if self.now_cck.saturating_sub(since) < HANDSHAKE_MIN_CCK {
            return;
        }
        let accepted = match &self.state {
            McuState::AwaitHandshake { kind, .. } => Some(Ok(*kind)),
            McuState::SyncAwaitHandshake { after, .. } => Some(Err(*after)),
            _ => None,
        };
        match accepted {
            Some(Ok(kind)) => self.state = self.after_handshake(kind),
            Some(Err(after)) => {
                // Synchronized: continue with what the sync was for.
                self.state = match after {
                    AfterSync::ResendLost(lost) => {
                        Self::sending(on_wire(STATUS_LAST_CODE_BAD), SentKind::ResyncBad { lost })
                    }
                    AfterSync::PowerUpStream => {
                        Self::sending(on_wire(STATUS_INIT_START), SentKind::PowerUpStart)
                    }
                };
            }
            None => {}
        }
    }

    fn sending(byte: u8, kind: SentKind) -> McuState {
        McuState::SendingByte {
            byte,
            kind,
            bit: 0,
            phase: BitPhase::DataSetup,
            next_edge_in_cck: BIT_PHASE_CCK,
        }
    }

    fn handshake_window_cck(kind: SentKind) -> u64 {
        match kind {
            SentKind::ResetWarnSecond => RESET_ACK_TIMEOUT_CCK,
            _ => RESYNC_TIMEOUT_CCK,
        }
    }

    /// Colour clocks until the MCU next needs to act (a KCLK edge or a
    /// timeout), for the emulator's idle fast-forward cap. None only
    /// when truly quiescent (idle with an empty buffer); waiting states
    /// still report their timeout deadlines so a stopped CPU cannot
    /// starve the resync or reset paths.
    pub fn next_event_cck(&self) -> Option<u32> {
        let cck = match &self.state {
            McuState::Idle => {
                if self.buffer.is_empty() {
                    return None;
                }
                1
            }
            McuState::PowerUpSelfTest { remaining_cck }
            | McuState::InterByteGap { remaining_cck }
            | McuState::HoldingReset { remaining_cck } => (*remaining_cck).max(1),
            McuState::SendingByte {
                next_edge_in_cck, ..
            }
            | McuState::SyncingBit {
                next_edge_in_cck, ..
            } => (*next_edge_in_cck).max(1),
            McuState::AwaitHandshake {
                kind, elapsed_cck, ..
            } => Self::handshake_window_cck(*kind)
                .saturating_sub(*elapsed_cck)
                .max(1),
            McuState::SyncAwaitHandshake { elapsed_cck, .. } => {
                RESYNC_TIMEOUT_CCK.saturating_sub(*elapsed_cck).max(1)
            }
        };
        Some(cck.min(u32::MAX as u64) as u32)
    }

    /// True when the MCU has nothing in flight: it is sitting in `Idle`
    /// with an empty key buffer and no handshake pulse being timed. While
    /// this holds, `tick` only advances `now_cck`, which no one reads until
    /// the next byte or handshake (both of which run with ticking active),
    /// so the whole tick can be skipped. This is the steady state whenever
    /// the user is not pressing keys.
    pub fn is_idle(&self) -> bool {
        matches!(self.state, McuState::Idle)
            && self.buffer.is_empty()
            && self.kdat_low_since.is_none()
    }

    /// Advance the MCU by `cck` colour clocks, driving CIA-A's CNT/SP
    /// pins. Returns true if a shifted bit asserted the CIA's IRQ line.
    pub fn tick(&mut self, cck: u32, cia_a: &mut Cia) -> bool {
        self.now_cck += cck as u64;
        let mut budget = cck as u64;
        let mut irq = false;
        loop {
            match &mut self.state {
                McuState::PowerUpSelfTest { remaining_cck } => {
                    if budget < *remaining_cck {
                        *remaining_cck -= budget;
                        break;
                    }
                    budget -= *remaining_cck;
                    // Self-test passed ($FC never sent: the emulated
                    // keyboard cannot fail); synchronize, then stream.
                    self.state = McuState::SyncingBit {
                        after: AfterSync::PowerUpStream,
                        phase: BitPhase::DataSetup,
                        next_edge_in_cck: BIT_PHASE_CCK,
                    };
                }
                McuState::Idle => {
                    let Some(byte) = self.buffer.pop_front() else {
                        break;
                    };
                    self.state = Self::sending(byte, SentKind::Normal);
                }
                McuState::InterByteGap { remaining_cck } => {
                    if budget < *remaining_cck {
                        *remaining_cck -= budget;
                        break;
                    }
                    budget -= *remaining_cck;
                    self.state = McuState::Idle;
                }
                McuState::SendingByte {
                    byte,
                    kind,
                    bit,
                    phase,
                    next_edge_in_cck,
                } => {
                    if budget < *next_edge_in_cck {
                        *next_edge_in_cck -= budget;
                        break;
                    }
                    budget -= *next_edge_in_cck;
                    *next_edge_in_cck = BIT_PHASE_CCK;
                    match phase {
                        BitPhase::DataSetup => {
                            cia_a.cnt_falling_edge();
                            *phase = BitPhase::ClkLow;
                        }
                        BitPhase::ClkLow => {
                            // Rising edge: the CIA samples the KDAT pin.
                            // The pin level equals the on-wire byte bit
                            // (on_wire folds in the active-low wire
                            // inversion).
                            let level = (*byte >> (7 - *bit)) & 1 != 0;
                            irq |= cia_a.cnt_rising_edge(level);
                            *phase = BitPhase::ClkHigh;
                        }
                        BitPhase::ClkHigh => {
                            *bit += 1;
                            if *bit == 8 {
                                // KDAT released; if the Amiga is already
                                // holding the line, the pulse started now.
                                self.state = McuState::AwaitHandshake {
                                    byte: *byte,
                                    kind: *kind,
                                    elapsed_cck: 0,
                                };
                            } else {
                                *phase = BitPhase::DataSetup;
                            }
                        }
                    }
                }
                McuState::AwaitHandshake {
                    byte,
                    kind,
                    elapsed_cck,
                } => {
                    let window = Self::handshake_window_cck(*kind);
                    let to_timeout = window.saturating_sub(*elapsed_cck);
                    if budget < to_timeout {
                        *elapsed_cck += budget;
                        break;
                    }
                    budget -= to_timeout;
                    let (kind, byte) = (*kind, *byte);
                    self.state = self.after_handshake_timeout(kind, byte);
                }
                McuState::SyncingBit {
                    after,
                    phase,
                    next_edge_in_cck,
                } => {
                    if budget < *next_edge_in_cck {
                        *next_edge_in_cck -= budget;
                        break;
                    }
                    budget -= *next_edge_in_cck;
                    *next_edge_in_cck = BIT_PHASE_CCK;
                    match phase {
                        BitPhase::DataSetup => {
                            cia_a.cnt_falling_edge();
                            *phase = BitPhase::ClkLow;
                        }
                        BitPhase::ClkLow => {
                            // The sync stream is logical 1s: KDAT driven
                            // low, so the pin level the CIA samples is 0.
                            irq |= cia_a.cnt_rising_edge(false);
                            *phase = BitPhase::ClkHigh;
                        }
                        BitPhase::ClkHigh => {
                            self.state = McuState::SyncAwaitHandshake {
                                after: *after,
                                elapsed_cck: 0,
                            };
                        }
                    }
                }
                McuState::SyncAwaitHandshake { after, elapsed_cck } => {
                    let to_timeout = RESYNC_TIMEOUT_CCK.saturating_sub(*elapsed_cck);
                    if budget < to_timeout {
                        *elapsed_cck += budget;
                        break;
                    }
                    budget -= to_timeout;
                    // Still no handshake: clock out another sync bit.
                    self.state = McuState::SyncingBit {
                        after: *after,
                        phase: BitPhase::DataSetup,
                        next_edge_in_cck: BIT_PHASE_CCK,
                    };
                }
                McuState::HoldingReset { remaining_cck } => {
                    // KCLK is held low for the whole hold.
                    cia_a.cnt_falling_edge();
                    if budget < *remaining_cck {
                        *remaining_cck -= budget;
                        break;
                    }
                    budget -= *remaining_cck;
                    self.system_reset_request = true;
                    self.state = McuState::PowerUpSelfTest {
                        remaining_cck: SELF_TEST_CCK,
                    };
                }
            }
        }
        irq
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chipset::cia::{Cia, Which, REG_CRA, REG_ICR, REG_SDR};

    const ICR_SP: u8 = 1 << 3;
    const CRA_SPMODE: u8 = 1 << 6;
    const BIT_CCK: u32 = (3 * BIT_PHASE_CCK) as u32;
    const BYTE_CCK: u32 = 8 * BIT_CCK;

    /// Software's view of a received byte: `ror.b` + `not.b` of SDR.
    fn decode(sdr: u8) -> u8 {
        (!sdr).rotate_right(1)
    }

    fn unmasked_cia() -> Cia {
        let mut cia = Cia::new(Which::A);
        cia.write(REG_ICR, 0x80 | ICR_SP);
        cia
    }

    /// A handshake pulse of `cck` device time, driven the way the Amiga
    /// does it: SPMODE up, time passes, SPMODE down.
    fn handshake(mcu: &mut KeyboardMcu, cia: &mut Cia, cck: u32) {
        cia.write(REG_CRA, CRA_SPMODE);
        mcu.amiga_kdat_edge(true);
        mcu.tick(cck, cia);
        cia.write(REG_CRA, 0);
        mcu.amiga_kdat_edge(false);
    }

    /// Walk a fresh MCU through self-test, sync, and the $FD/$FE
    /// power-up stream, leaving it Idle. Returns the decoded stream.
    fn complete_power_up(mcu: &mut KeyboardMcu, cia: &mut Cia) -> Vec<u8> {
        let mut stream = Vec::new();
        mcu.tick(SELF_TEST_CCK as u32, cia);
        // Sync bit, then its handshake.
        mcu.tick(BIT_CCK, cia);
        handshake(mcu, cia, HANDSHAKE_MIN_CCK as u32);
        // Stream bytes until $FE has been sent and handshaked.
        for _ in 0..32 {
            if mcu.tick(2 * BYTE_CCK, cia) {
                let byte = decode(cia.read(REG_SDR));
                stream.push(byte);
                cia.read(REG_ICR);
                handshake(mcu, cia, HANDSHAKE_MIN_CCK as u32);
                if byte == STATUS_INIT_END {
                    break;
                }
            }
        }
        // Drain the inter-byte gap left by the final handshake.
        mcu.tick(2 * BYTE_CCK, cia);
        stream
    }

    /// A power-up-completed MCU ready for normal key traffic.
    fn idle_mcu(cia: &mut Cia) -> KeyboardMcu {
        let mut mcu = KeyboardMcu::new();
        let stream = complete_power_up(&mut mcu, cia);
        assert_eq!(stream, vec![STATUS_INIT_START, STATUS_INIT_END]);
        mcu
    }

    #[test]
    fn encoding_distinguishes_press_and_release_and_masks_range() {
        assert_eq!(encode_keyboard_byte(0x01, true), 0xFD);
        assert_eq!(encode_keyboard_byte(0x01, false), 0xFC);
        assert_eq!(
            encode_keyboard_byte(0x81, true),
            encode_keyboard_byte(0x01, true)
        );
        // Status codes are rotated on the wire like everything else.
        assert_eq!(decode(on_wire(STATUS_INIT_START)), STATUS_INIT_START);
        assert_eq!(decode(on_wire(STATUS_RESET_WARNING)), STATUS_RESET_WARNING);
    }

    #[test]
    fn power_up_streams_fd_then_fe_after_sync() {
        let mut cia = unmasked_cia();
        let mut mcu = KeyboardMcu::new();
        let stream = complete_power_up(&mut mcu, &mut cia);
        assert_eq!(stream, vec![STATUS_INIT_START, STATUS_INIT_END]);
    }

    #[test]
    fn power_up_reports_keys_already_held() {
        let mut cia = unmasked_cia();
        let mut mcu = KeyboardMcu::new();
        // Keys pressed while the machine boots land in the stream.
        mcu.key_transition(0x20, true);
        mcu.key_transition(0x21, true);
        let stream = complete_power_up(&mut mcu, &mut cia);
        assert_eq!(stream, vec![STATUS_INIT_START, 0x20, 0x21, STATUS_INIT_END]);
    }

    #[test]
    fn power_up_stream_marches_on_without_handshakes() {
        let mut cia = unmasked_cia();
        let mut mcu = KeyboardMcu::new();
        mcu.tick(SELF_TEST_CCK as u32, &mut cia);
        mcu.tick(BIT_CCK, &mut cia);
        handshake(&mut mcu, &mut cia, HANDSHAKE_MIN_CCK as u32);
        // $FD transmits; nobody handshakes it. After the 143 ms window
        // the stream continues regardless (firmware ignores the result).
        assert!(mcu.tick(2 * BYTE_CCK, &mut cia));
        assert_eq!(decode(cia.read(REG_SDR)), STATUS_INIT_START);
        cia.read(REG_ICR);
        assert!(mcu.tick(RESYNC_TIMEOUT_CCK as u32 + 2 * BYTE_CCK, &mut cia));
        assert_eq!(decode(cia.read(REG_SDR)), STATUS_INIT_END);
    }

    #[test]
    fn byte_arrives_in_sdr_only_after_eight_bit_times() {
        let mut cia = unmasked_cia();
        let mut mcu = idle_mcu(&mut cia);
        mcu.key_transition(0x01, true);

        // 7 full bit times: no SP interrupt yet.
        let mut irq = false;
        for _ in 0..7 {
            irq |= mcu.tick(BIT_CCK, &mut cia);
        }
        assert!(!irq, "SP fired before the 8th bit");
        // The 8th bit completes the byte.
        assert!(mcu.tick(BIT_CCK, &mut cia));
        assert_eq!(cia.read(REG_SDR), 0xFD);
    }

    #[test]
    fn one_large_tick_spans_a_whole_byte() {
        let mut cia = unmasked_cia();
        let mut mcu = idle_mcu(&mut cia);
        mcu.key_transition(0x35, false);
        assert!(mcu.tick(2 * BYTE_CCK, &mut cia));
        assert_eq!(cia.read(REG_SDR), encode_keyboard_byte(0x35, false));
    }

    #[test]
    fn next_byte_waits_for_a_long_enough_handshake() {
        let mut cia = unmasked_cia();
        let mut mcu = idle_mcu(&mut cia);
        mcu.key_transition(0x01, true);
        mcu.key_transition(0x02, true);
        assert!(mcu.tick(2 * BYTE_CCK, &mut cia));
        assert_eq!(cia.read(REG_SDR), 0xFD);
        // Software acknowledges the byte (clears ICR / the IR line).
        cia.read(REG_ICR);

        // 84 us pulse: ignored, the second byte must not transmit.
        handshake(&mut mcu, &mut cia, (us_to_cck(84)) as u32);
        assert!(!mcu.tick(2 * BYTE_CCK, &mut cia));
        assert_eq!(cia.read(REG_SDR), 0xFD);

        // 85 us pulse: accepted; the second byte follows.
        handshake(&mut mcu, &mut cia, HANDSHAKE_MIN_CCK as u32);
        assert!(mcu.tick(2 * BYTE_CCK, &mut cia));
        assert_eq!(cia.read(REG_SDR), 0xFB);
    }

    #[test]
    fn mid_byte_spmode_pulse_loses_bits_as_on_hardware() {
        let mut cia = unmasked_cia();
        let mut mcu = idle_mcu(&mut cia);
        mcu.key_transition(0x01, true);
        // Software driving SPMODE output mid-byte puts the CIA shifter
        // in output mode: the KCLK edges that land during the pulse are
        // discarded, so the byte never completes -- the keyboard then
        // recovers through the resync path (covered separately).
        mcu.tick(3 * BIT_CCK, &mut cia);
        handshake(&mut mcu, &mut cia, HANDSHAKE_MIN_CCK as u32 + 10);
        assert!(!mcu.tick(8 * BIT_CCK, &mut cia), "byte must not complete");
        assert_ne!(cia.read(REG_SDR), 0xFD);
        // The MCU is now waiting for a handshake that will not come;
        // its next deadline is the resync timeout.
        assert!(mcu.next_event_cck().is_some());
    }

    #[test]
    fn missing_handshake_resyncs_with_f9_and_retransmission() {
        let mut cia = unmasked_cia();
        let mut mcu = idle_mcu(&mut cia);
        mcu.key_transition(0x01, true);
        assert!(mcu.tick(2 * BYTE_CCK, &mut cia));
        assert_eq!(cia.read(REG_SDR), 0xFD);

        // 143 ms with no handshake: the keyboard clocks a single sync
        // bit (one bit time) and waits again.
        mcu.tick(RESYNC_TIMEOUT_CCK as u32, &mut cia);
        mcu.tick(BIT_CCK, &mut cia);
        // Two more timeout rounds keep clocking lone bits, not bytes.
        mcu.tick(RESYNC_TIMEOUT_CCK as u32, &mut cia);
        mcu.tick(BIT_CCK, &mut cia);

        // The sync bits shifted into the CIA garble SDR; software acks
        // whatever arrived, as keyboard.device does.
        cia.read(REG_ICR);

        // Handshake one sync bit: $F9 arrives...
        handshake(&mut mcu, &mut cia, HANDSHAKE_MIN_CCK as u32);
        mcu.tick(2 * BYTE_CCK, &mut cia);
        assert_eq!(decode(cia.read(REG_SDR)), STATUS_LAST_CODE_BAD);
        cia.read(REG_ICR);
        // ...and after its handshake, the lost key is retransmitted.
        handshake(&mut mcu, &mut cia, HANDSHAKE_MIN_CCK as u32);
        assert!(mcu.tick(2 * BYTE_CCK, &mut cia));
        assert_eq!(cia.read(REG_SDR), 0xFD);
    }

    #[test]
    fn reset_chord_sends_two_warnings_then_holds_kclk() {
        let mut cia = unmasked_cia();
        let mut mcu = idle_mcu(&mut cia);
        mcu.key_transition(RAWKEY_CTRL, true);
        mcu.key_transition(RAWKEY_LEFT_AMIGA, true);
        mcu.key_transition(RAWKEY_RIGHT_AMIGA, true);

        // First $78, handshaked (any buffered chord presses are
        // discarded when the chord lands).
        assert!(mcu.tick(2 * BYTE_CCK, &mut cia));
        assert_eq!(decode(cia.read(REG_SDR)), STATUS_RESET_WARNING);
        cia.read(REG_ICR);
        handshake(&mut mcu, &mut cia, HANDSHAKE_MIN_CCK as u32);

        // Second $78, acknowledged.
        assert!(mcu.tick(2 * BYTE_CCK, &mut cia));
        assert_eq!(decode(cia.read(REG_SDR)), STATUS_RESET_WARNING);
        cia.read(REG_ICR);
        handshake(&mut mcu, &mut cia, HANDSHAKE_MIN_CCK as u32);

        // KCLK is now held low; the reset fires after 500 ms.
        assert!(!mcu.take_system_reset_request());
        mcu.tick(KCLK_RESET_HOLD_CCK as u32 / 2, &mut cia);
        assert!(!mcu.take_system_reset_request());
        mcu.tick(KCLK_RESET_HOLD_CCK as u32, &mut cia);
        assert!(mcu.take_system_reset_request());
        // The MCU restarts its own power-up flow (already inside it,
        // since the tick's leftover budget ran into the self-test).
        assert!(mcu.next_event_cck().is_some());
    }

    #[test]
    fn unanswered_reset_warning_still_resets() {
        let mut cia = unmasked_cia();
        let mut mcu = idle_mcu(&mut cia);
        mcu.key_transition(RAWKEY_CTRL, true);
        mcu.key_transition(RAWKEY_LEFT_AMIGA, true);
        mcu.key_transition(RAWKEY_RIGHT_AMIGA, true);

        // $78 transmits but nobody handshakes: after the wait the
        // keyboard resets the machine anyway.
        mcu.tick(2 * BYTE_CCK, &mut cia);
        mcu.tick(RESYNC_TIMEOUT_CCK as u32, &mut cia);
        assert!(!mcu.take_system_reset_request());
        mcu.tick(KCLK_RESET_HOLD_CCK as u32 + 1, &mut cia);
        assert!(mcu.take_system_reset_request());
    }

    #[test]
    fn begin_power_up_rebuffers_held_keys() {
        let mut cia = unmasked_cia();
        let mut mcu = idle_mcu(&mut cia);
        mcu.key_transition(0x40, true); // space held across the reset
        mcu.tick(2 * BYTE_CCK, &mut cia); // its press transmits
        cia.read(REG_ICR);

        mcu.begin_power_up();
        let stream = complete_power_up(&mut mcu, &mut cia);
        assert_eq!(stream, vec![STATUS_INIT_START, 0x40, STATUS_INIT_END]);
    }

    #[test]
    fn next_event_deadline_is_reported_in_every_state() {
        let mut cia = unmasked_cia();
        let fresh = KeyboardMcu::new();
        // Self-test: its remaining time is the deadline.
        assert_eq!(fresh.next_event_cck(), Some(SELF_TEST_CCK as u32));

        let mut mcu = idle_mcu(&mut cia);
        // Quiescent: nothing scheduled.
        assert_eq!(mcu.next_event_cck(), None);
        mcu.key_transition(0x01, true);
        // Idle with a buffered byte: act now.
        assert_eq!(mcu.next_event_cck(), Some(1));
        // Mid-bit: the next KCLK edge.
        mcu.tick(10, &mut cia);
        let edge = mcu.next_event_cck().expect("bit-edge deadline");
        assert!(edge > 0 && edge as u64 <= BIT_PHASE_CCK, "edge {edge}");
        // Awaiting handshake: the resync timeout keeps a deadline alive.
        mcu.tick(2 * BYTE_CCK, &mut cia);
        let deadline = mcu.next_event_cck().expect("timeout deadline");
        assert!(deadline as u64 <= RESYNC_TIMEOUT_CCK);
        // Through the resync timeout: the lone sync bit clocks out and
        // the MCU parks waiting for its handshake -- a deadline exists
        // in both of those states too (residual budget from the earlier
        // ticks decides which one we land in).
        mcu.tick(RESYNC_TIMEOUT_CCK as u32, &mut cia);
        let next = mcu.next_event_cck().expect("resync deadline");
        assert!(next > 0 && next as u64 <= RESYNC_TIMEOUT_CCK, "next {next}");
    }

    /// Drain every buffered event over the wire, handshaking each one;
    /// returns the decoded byte sequence.
    fn drain_stream(mcu: &mut KeyboardMcu, cia: &mut Cia) -> Vec<u8> {
        let mut stream = Vec::new();
        for _ in 0..32 {
            if !mcu.tick(2 * BYTE_CCK, cia) {
                break;
            }
            stream.push(decode(cia.read(REG_SDR)));
            cia.read(REG_ICR);
            handshake(mcu, cia, HANDSHAKE_MIN_CCK as u32);
        }
        stream
    }

    #[test]
    fn caps_lock_toggles_with_press_and_release_codes() {
        let mut cia = unmasked_cia();
        let mut mcu = idle_mcu(&mut cia);

        // First press: LED on, press code; physical release silent.
        mcu.key_transition(RAWKEY_CAPS_LOCK, true);
        assert!(mcu.caps_lock_led());
        mcu.key_transition(RAWKEY_CAPS_LOCK, false);
        // Second press: LED off, release code; release silent again.
        mcu.key_transition(RAWKEY_CAPS_LOCK, true);
        assert!(!mcu.caps_lock_led());
        mcu.key_transition(RAWKEY_CAPS_LOCK, false);

        assert_eq!(
            drain_stream(&mut mcu, &mut cia),
            vec![RAWKEY_CAPS_LOCK, RAWKEY_CAPS_LOCK | 0x80]
        );
    }

    #[test]
    fn type_ahead_overflow_reports_fa_once_room_frees() {
        let mut cia = unmasked_cia();
        let mut mcu = idle_mcu(&mut cia);
        // Fill the 10-event buffer, then two more that are lost.
        for key in 0..12u8 {
            mcu.key_transition(key, true);
        }
        let mut expected: Vec<u8> = (0..10u8).collect();
        // Drain everything: after the 10 buffered events the next
        // enqueued event is preceded by $FA (buffer overflow).
        let first = drain_stream(&mut mcu, &mut cia);
        assert_eq!(first, expected);
        mcu.key_transition(0x20, true);
        expected = vec![STATUS_OVERFLOW, 0x20];
        assert_eq!(drain_stream(&mut mcu, &mut cia), expected);
    }

    #[test]
    fn ghost_completing_press_is_suppressed_until_unambiguous() {
        let mut cia = unmasked_cia();
        let mut mcu = idle_mcu(&mut cia);
        // Q (col 2, row 13), W (col 2, row 12), A (col 3, row 13) are
        // three corners of a matrix rectangle; S (col 3, row 12) is the
        // fourth and cannot be distinguished from its phantom.
        mcu.key_transition(0x10, true); // Q
        mcu.key_transition(0x11, true); // W
        mcu.key_transition(0x20, true); // A
        mcu.key_transition(0x21, true); // S: suppressed
        assert_eq!(drain_stream(&mut mcu, &mut cia), vec![0x10, 0x11, 0x20]);

        // Releasing W clears the ambiguity: S appears as a fresh press.
        mcu.key_transition(0x11, false);
        assert_eq!(drain_stream(&mut mcu, &mut cia), vec![0x11 | 0x80, 0x21]);
    }

    #[test]
    fn ghost_suppressed_key_released_early_stays_silent() {
        let mut cia = unmasked_cia();
        let mut mcu = idle_mcu(&mut cia);
        mcu.key_transition(0x10, true); // Q
        mcu.key_transition(0x11, true); // W
        mcu.key_transition(0x20, true); // A
        mcu.key_transition(0x21, true); // S: suppressed
        mcu.key_transition(0x21, false); // released while suppressed
        mcu.key_transition(0x11, false);
        assert_eq!(
            drain_stream(&mut mcu, &mut cia),
            vec![0x10, 0x11, 0x20, 0x11 | 0x80]
        );
    }

    #[test]
    fn qualifiers_never_ghost() {
        let mut cia = unmasked_cia();
        let mut mcu = idle_mcu(&mut cia);
        // Shift+Alt plus two matrix keys: qualifiers are on dedicated
        // lines, so nothing is suppressed.
        mcu.key_transition(0x60, true); // left shift
        mcu.key_transition(0x64, true); // left alt
        mcu.key_transition(0x10, true); // Q
        mcu.key_transition(0x21, true); // S
        assert_eq!(
            drain_stream(&mut mcu, &mut cia),
            vec![0x60, 0x64, 0x10, 0x21]
        );
    }

    #[test]
    fn handshake_pulse_straddling_byte_end_counts_full_width() {
        // The boot ROM handshake pulse is barely over the 85 us minimum
        // and its interrupt handler can drive KDAT low while the byte's
        // final bit cell is still clocking out. The 6500/1 measures the
        // line, not the protocol state: the full pulse width counts.
        let mut cia = unmasked_cia();
        let mut mcu = idle_mcu(&mut cia);
        mcu.key_transition(0x01, true);
        mcu.key_transition(0x02, true);

        // Stop 40 cck short of the byte's final edge.
        mcu.tick(2 * BYTE_CCK - 40, &mut cia);
        // The Amiga drives KDAT low now, mid-final-bit...
        cia.write(REG_CRA, CRA_SPMODE);
        mcu.amiga_kdat_edge(true);
        // ...the byte completes 40 cck later, and the pulse is released
        // 280 cck after that: 320 cck total, >= the 301 cck minimum,
        // even though only 280 fell inside AwaitHandshake.
        mcu.tick(40, &mut cia);
        mcu.tick(280, &mut cia);
        cia.write(REG_CRA, 0);
        mcu.amiga_kdat_edge(false);

        cia.read(REG_SDR);
        cia.read(REG_ICR);
        assert!(mcu.tick(2 * BYTE_CCK, &mut cia), "second byte must follow");
        assert_eq!(cia.read(REG_SDR), 0xFB);
    }
}
