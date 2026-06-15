// SPDX-License-Identifier: GPL-3.0-or-later

//! Paula-side state: serial UART registers, interrupt enable/request,
//! and four-channel audio DMA + mixer.

use crate::audio::{AudioRuntimeStatus, AudioSink, MIX_SAMPLE_RATE};
use crate::drive_sounds::DriveSounds;
use crate::serial::SerialSink;
use std::f32::consts::PI;

/// PAL Paula audio clock. The standard sample-rate-from-period formula
/// is `PAULA_CLOCK_HZ / AUDxPER`, e.g. period 254 -> ~13977 Hz. One
/// Paula audio clock tick equals one Amiga color clock for this
/// emulator's device timeline.
pub const PAULA_CLOCK_HZ: u32 = 3_546_895;
pub const PAL_AUDIO_MIN_PERIOD_CCK: u16 = 123;
pub const NTSC_AUDIO_MIN_PERIOD_CCK: u16 = 124;

/// CIA-A IRQ line (keyboard, timers, parallel handshake).
pub const INT_PORTS: u16 = 1 << 3;
pub const INT_VERTB: u16 = 1 << 5;
/// CIA-B IRQ line (disk drives, serial port DCD/CTS).
pub const INT_EXTER: u16 = 1 << 13;
/// Bit 14 is the master interrupt enable in INTENA. The same bit is
/// also latchable in INTREQ as the undocumented INT14 source.
pub const INT_MASTER: u16 = 1 << 14;
pub const INT_INT14: u16 = 1 << 14;

pub const INT_TBE: u16 = 1 << 0;
pub const INT_DSKBLK: u16 = 1 << 1;
// Named for completeness of the INTREQ bit set; nothing raises it yet.
#[allow(dead_code)]
pub const INT_SOFT: u16 = 1 << 2;
pub const INT_COPER: u16 = 1 << 4;
pub const INT_BLIT: u16 = 1 << 6;

/// Per-channel audio interrupt bits in INTENA/INTREQ. Raised on the
/// DMA-enable edge (so the CPU can prime the *next* buffer's LC/LEN
/// before the current one has even played) and again every time a
/// buffer completes and auto-restarts from the latched LC/LEN.
pub const INT_AUD0: u16 = 1 << 7;
pub const INT_AUD1: u16 = 1 << 8;
pub const INT_AUD2: u16 = 1 << 9;
pub const INT_AUD3: u16 = 1 << 10;
pub const INT_RBF: u16 = 1 << 11;
pub const INT_DSKSYNC: u16 = 1 << 12;
const INT_AUDX: [u16; 4] = [INT_AUD0, INT_AUD1, INT_AUD2, INT_AUD3];
const INTREQ_MASK: u16 = 0x7FFF;
const SERPER_LONG: u16 = 1 << 15;
const ADKCON_UARTBRK: u16 = 1 << 11;

/// DMACON.DMAEN master enable. Stored on agnus.dmacon; Paula audio
/// gating ANDs this with the per-channel AUDxEN bits 0..3.
pub const DMACON_DMAEN: u16 = 1 << 9;
const LED_FILTER_CUTOFF_HZ: f32 = 4_000.0;
const POT_COUNTER_CCK: u32 = 512;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
enum ChanState {
    /// DMA disabled. Output is silent (current = 0).
    Off,
    /// CPU-driven AUDxDAT playback with DMA disabled. The loaded word
    /// emits high byte then low byte at AUDxPER cadence and then waits
    /// for another CPU write.
    Manual,
    /// A direct AUDxDAT word has completed. Paula keeps the last output
    /// sample stable until software writes another word or DMA starts.
    ManualHold,
    /// DMA just enabled. On the next tick we fetch the first word,
    /// and the previous transition has already raised the channel's
    /// AUDxx interrupt (so software can pre-stage the next buffer).
    StartPending,
    /// Steady-state playback. Period accumulator counts color clocks
    /// until the next byte is emitted.
    Running,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioDmaRequest {
    pub address: u32,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct AudChannel {
    // CPU-visible latches (set via MMIO writes).
    lc: u32,
    len: u16,
    per: u16,
    vol: u8,

    // Live playback state.
    ptr: u32,
    words_left: u32,
    word_hi: i8,
    word_lo: i8,
    phase: u8, // 0 = next emit is word_hi, 1 = next emit is word_lo
    period_acc: u32,
    dma_fetch_cooldown_cck: u32,
    current: i8,
    state: ChanState,
    dat_latch: u16,
    manual_pending: bool,
    dma_request: bool,
    /// Set when the length counter underflowed and the channel raised its
    /// interrupt, but the AUDxLC/AUDxLEN reload for the next buffer has
    /// not been applied yet. Real Paula leaves a one-word gap between the
    /// interrupt and the pointer reload, which is what lets a one-shot
    /// sample's IRQ handler rewrite AUDxLC/AUDxLEN to a silence loop
    /// before Paula latches it. The reload is applied when the channel
    /// arms its next DMA fetch (see `arm_next_buffer_fetch`), reading
    /// whatever AUDxLC/AUDxLEN the handler has by then written.
    restart_pending: bool,
    /// One-word fetch-ahead holding register. Audio DMA reads the next
    /// sample word into here during the current word's playback (one
    /// word ahead), so the word is ready the moment the active word is
    /// exhausted. Real Paula does the same: each channel's fixed
    /// per-line DMA slot fills an internal buffer one word ahead of the
    /// output shifter. Without it, a single-word buffer cannot be
    /// refilled in time at short periods (a word at period 124..=171
    /// lasts fewer color clocks than the gap to the channel's next slot
    /// once it has been consumed), so samples repeat and the pitch
    /// drops.
    next_word: Option<u16>,
}

impl AudChannel {
    fn new() -> Self {
        Self {
            lc: 0,
            len: 0,
            per: 0,
            vol: 0,
            ptr: 0,
            words_left: 0,
            word_hi: 0,
            word_lo: 0,
            phase: 0,
            period_acc: 0,
            dma_fetch_cooldown_cck: 0,
            current: 0,
            state: ChanState::Off,
            dat_latch: 0,
            manual_pending: false,
            dma_request: false,
            restart_pending: false,
            next_word: None,
        }
    }

    fn audio_len_words(len: u16) -> u32 {
        // AUDxLEN = 0 means "65536 words" on real hardware. We treat
        // 0 as 65536 to match.
        if len == 0 {
            0x1_0000
        } else {
            len as u32
        }
    }

    /// Latch the location/length registers into the live playback
    /// pointer and length counter. Run on the DMA-enable edge and again
    /// whenever the length counter underflows (loop restart). Reading
    /// `lc`/`len` here -- not at some earlier capture -- matches Paula:
    /// the pointer reload at underflow uses whatever AUDxLC/AUDxLEN the
    /// CPU has written by then, which is how the documented "set the
    /// next buffer in the audio interrupt handler" double-buffering
    /// works.
    fn reload_buffer(&mut self, ptr_mask: u32) {
        self.ptr = self.lc & ptr_mask;
        self.words_left = Self::audio_len_words(self.len);
    }

    fn reset_dma_start_timing(&mut self) {
        self.phase = 0;
        self.period_acc = 0;
        self.dma_fetch_cooldown_cck = 0;
        self.next_word = None;
        self.restart_pending = false;
    }

    fn request_dma(&mut self) {
        self.dma_request = true;
    }

    fn clear_dma_request(&mut self) {
        self.dma_request = false;
    }

    /// Move the prefetched word from the fetch-ahead holding register
    /// into the active sample buffer. Returns false when the holding
    /// register is empty (DMA could not keep up), in which case the
    /// active word is left in place and its samples repeat.
    fn promote_next_word(&mut self) -> bool {
        match self.next_word.take() {
            Some(word) => {
                self.word_hi = (word >> 8) as u8 as i8;
                self.word_lo = word as u8 as i8;
                true
            }
            None => false,
        }
    }

    fn latch_manual_word(&mut self) {
        self.word_hi = (self.dat_latch >> 8) as u8 as i8;
        self.word_lo = self.dat_latch as u8 as i8;
        self.current = self.word_hi;
        self.phase = 1;
        self.period_acc = 0;
        self.state = ChanState::Manual;
        self.manual_pending = false;
        self.next_word = None;
        self.clear_dma_request();
    }

    /// Accept a DMA word into the fetch-ahead holding register and
    /// advance the playback pointer/length counter. Returns true when
    /// this read made the length counter underflow (the buffer's last
    /// word was just fetched), which raises the channel interrupt and
    /// reloads the pointer for the next loop.
    fn accept_dma_word(&mut self, word: u16, ptr_mask: u32) -> bool {
        let _ = ptr_mask;
        self.next_word = Some(word);
        self.ptr = self.ptr.wrapping_add(2) & ptr_mask;
        if self.words_left > 0 {
            self.words_left -= 1;
        }
        if self.words_left == 0 {
            // Length underflow: raise the channel interrupt now, but defer
            // the AUDxLC/AUDxLEN reload to the next buffer fetch. Real
            // Paula has a one-word gap here, which is the window a one-shot
            // sample's interrupt handler uses to point AUDxLC/AUDxLEN at a
            // silence loop before Paula latches it. Reloading immediately
            // (with the still-current AUDxLC) replays the whole sample --
            // an audible echo on effect/voice channels.
            self.restart_pending = true;
            true
        } else {
            false
        }
    }

    /// Arm the next audio DMA fetch. If the previous buffer just
    /// underflowed, apply the deferred AUDxLC/AUDxLEN reload first, reading
    /// whatever the channel's interrupt handler has written by now.
    fn arm_next_buffer_fetch(&mut self, ptr_mask: u32) {
        if self.restart_pending {
            self.reload_buffer(ptr_mask);
            self.restart_pending = false;
        }
        self.request_dma();
    }

    fn next_irq_cck(&self) -> Option<u32> {
        let period = u32::from(self.per);
        if period == 0 {
            return None;
        }
        // The channel interrupt fires when the length counter underflows
        // as the buffer's last word is *fetched* into the holding
        // register -- one full word (two sample periods) before that word
        // would finish playing. Predict the playout position of the last
        // sample, then pull the deadline back by that one-word fetch-ahead
        // lead so callers bounding work near the IRQ stop in time.
        let periods = match self.state {
            ChanState::Off => return None,
            ChanState::ManualHold => return None,
            ChanState::Manual => return None,
            ChanState::StartPending => {
                let words_after_initial_fetch = self.words_left.saturating_sub(1);
                1u32.saturating_add(words_after_initial_fetch.saturating_mul(2))
                    .saturating_sub(2)
            }
            ChanState::Running => {
                // After a length underflow the AUDxLC/AUDxLEN reload is
                // deferred to the next fetch, so `words_left` is momentarily
                // zero; the buffer about to be (re)loaded holds AUDxLEN
                // words, so predict against that instead.
                let words_left = if self.restart_pending {
                    Self::audio_len_words(self.len)
                } else {
                    self.words_left
                };
                let word_periods = words_left.saturating_mul(2);
                let playout = if self.phase == 1 {
                    1u32.saturating_add(word_periods)
                } else {
                    2u32.saturating_add(word_periods)
                };
                playout.saturating_sub(2)
            }
        };
        let first_period = period.saturating_sub(self.period_acc).max(1);
        let deadline =
            first_period.saturating_add(periods.saturating_sub(1).saturating_mul(period));
        Some(deadline.max(self.dma_fetch_cooldown_cck).max(1))
    }
}

fn paula_volume_from_word(word: u16) -> u8 {
    ((word & 0x007F) as u8).min(64)
}

#[cfg(test)]
fn read_chip_word_for_audio_test(chip_ram: &[u8], address: u32) -> u16 {
    if chip_ram.is_empty() {
        return 0;
    }
    let off = (address as usize) % chip_ram.len();
    let hi = chip_ram[off] as u16;
    let lo = chip_ram[(off + 1) % chip_ram.len()] as u16;
    (hi << 8) | lo
}

#[derive(Clone, Copy, serde::Serialize, serde::Deserialize)]
struct SerialTxShift {
    word: u16,
    long: bool,
    bit_cck: u32,
    remaining_cck: u32,
    bit_index: u8,
    total_bits: u8,
    break_seen: bool,
}

#[derive(Clone, Copy, serde::Serialize, serde::Deserialize)]
struct SerialRxShift {
    word: u16,
    long: bool,
    bit_cck: u32,
    remaining_cck: u32,
    bit_index: u8,
    total_bits: u8,
}

#[derive(Debug, Clone, Copy)]
pub struct PotPins {
    pub left_x_released: bool,
    pub left_y_released: bool,
    pub right_x_released: bool,
    pub right_y_released: bool,
}

#[derive(Clone, Copy)]
struct AudioModEvent {
    source: usize,
    word: u16,
    word_start: bool,
}

/// CD audio stream from the CD controller to the host mixer. CD-DA is
/// 44.1 kHz stereo, exactly the mixer rate, so the controller pushes one
/// decoded sector (588 frames) per CD frame and the mixer pops one
/// sample pair per output frame; both sides advance on emulated time, so
/// they stay in step. Bounded so a stalled consumer cannot grow it.
#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct CdAudioRing {
    samples: std::collections::VecDeque<(f32, f32)>,
}

/// 32 sectors (~0.43 s) of buffered CD audio.
const CD_AUDIO_RING_LIMIT: usize = 32 * 588;

impl CdAudioRing {
    /// Decode one 2352-byte CD-DA sector (s16le interleaved stereo) into
    /// the ring. Returns false (dropping the sector) when full.
    pub fn push_sector(&mut self, sector: &[u8]) -> bool {
        if self.samples.len() + sector.len() / 4 > CD_AUDIO_RING_LIMIT {
            return false;
        }
        for frame in sector.chunks_exact(4) {
            let left = i16::from_le_bytes([frame[0], frame[1]]);
            let right = i16::from_le_bytes([frame[2], frame[3]]);
            self.samples
                .push_back((f32::from(left) / 32768.0, f32::from(right) / 32768.0));
        }
        true
    }

    /// Room for at least one more sector?
    pub fn wants_sector(&self) -> bool {
        self.samples.len() + 588 <= CD_AUDIO_RING_LIMIT
    }

    pub fn next_sample(&mut self) -> (f32, f32) {
        self.samples.pop_front().unwrap_or((0.0, 0.0))
    }

    pub fn clear(&mut self) {
        self.samples.clear();
    }
}

fn null_serial_sink() -> Box<dyn SerialSink> {
    Box::new(crate::serial::NullSerialSink)
}

fn null_audio_sink() -> Box<dyn AudioSink> {
    Box::new(crate::audio::NullSink)
}

#[derive(serde::Serialize, serde::Deserialize)]
pub struct Paula {
    pub serper: u16,
    pub intena: u16,
    pub intreq: u16,
    // Host-side sinks, not emulated state: a save state skips them and the
    // loader moves the live sinks across to the restored Paula.
    #[serde(skip, default = "null_serial_sink")]
    pub serial: Box<dyn SerialSink>,
    #[serde(skip, default = "null_audio_sink")]
    pub audio: Box<dyn AudioSink>,
    pub adkcon: u16,
    pub potgo: u16,

    chans: [AudChannel; 4],
    serial_tx_buffer: Option<u16>,
    serial_tx_shift: Option<SerialTxShift>,
    serial_rx_shift: Option<SerialRxShift>,
    serial_rx_buffer: Option<u16>,
    serial_overrun: bool,
    serial_rx_pin_high: bool,
    serial_rx_sync_0_high: bool,
    serial_rx_sync_1_high: bool,
    serial_tx_pin_high: bool,
    pot_counters: [u8; 4],
    pot_running: bool,
    pot_acc_cck: u32,

    // DMACON value as seen last tick; used to edge-detect AUDxEN
    // transitions so we can raise the AUDxx IRQ on the rising edge.
    last_dmacon: u16,

    // Mixer host-sample accumulator in units of
    // color-clocks * MIX_SAMPLE_RATE. One output frame is due each
    // time this reaches PAULA_CLOCK_HZ.
    host_sample_acc: u64,

    led_filter_enabled: bool,
    led_filter: StereoLedFilter,
    output_volume: f32,
    // CD audio samples streamed by the CD controller (CD32 Akiko), mixed
    // into the host output at the shared 44.1 kHz mixer rate.
    cd_audio: CdAudioRing,
    // Synthesized floppy-drive noises (motor/seek/read), mixed into the
    // host frames after the LED filter: the drive is an acoustic source
    // beside the machine, not part of Paula's filtered audio path.
    drive_sounds: DriveSounds,
    dma_addr_mask: u32,
    audio_mod_next_period: [bool; 4],
    audio_min_period_cck: u16,
    // Optional host-side recording tap: when Some, every mixed stereo
    // frame is also appended here (before the master output volume, so
    // recordings stay full scale regardless of the volume slider) for
    // the window's video recorder to drain once per emulated frame.
    capture: Option<Vec<(f32, f32)>>,
}

impl Paula {
    pub fn new(serial: Box<dyn SerialSink>, audio: Box<dyn AudioSink>) -> Self {
        Self {
            serper: 0,
            intena: 0,
            intreq: 0,
            serial,
            audio,
            adkcon: 0,
            potgo: 0,
            chans: [
                AudChannel::new(),
                AudChannel::new(),
                AudChannel::new(),
                AudChannel::new(),
            ],
            serial_tx_buffer: None,
            serial_tx_shift: None,
            serial_rx_shift: None,
            serial_rx_buffer: None,
            serial_overrun: false,
            serial_rx_pin_high: true,
            serial_rx_sync_0_high: true,
            serial_rx_sync_1_high: true,
            serial_tx_pin_high: true,
            pot_counters: [0; 4],
            pot_running: false,
            pot_acc_cck: 0,
            last_dmacon: 0,
            host_sample_acc: 0,
            led_filter_enabled: true,
            led_filter: StereoLedFilter::new(),
            output_volume: 1.0,
            cd_audio: CdAudioRing::default(),
            drive_sounds: DriveSounds::new(),
            dma_addr_mask: 0x001F_FFFF,
            audio_mod_next_period: [false; 4],
            audio_min_period_cck: PAL_AUDIO_MIN_PERIOD_CCK,
            capture: None,
        }
    }

    /// Enable or disable the recording tap. Enabling starts with an
    /// empty buffer; disabling discards anything not yet drained.
    pub fn set_audio_capture_enabled(&mut self, enabled: bool) {
        self.capture = enabled.then(Vec::new);
    }

    /// Drain the mixed stereo frames captured since the last call.
    /// Returns an empty Vec when the tap is disabled.
    pub fn take_captured_audio(&mut self) -> Vec<(f32, f32)> {
        match &mut self.capture {
            Some(buf) => std::mem::take(buf),
            None => Vec::new(),
        }
    }

    pub fn set_dma_addr_mask(&mut self, mask: u32) {
        self.dma_addr_mask = mask | 1;
        let ptr_mask = self.dma_ptr_mask();
        for ch in &mut self.chans {
            ch.lc &= ptr_mask;
            ch.ptr &= ptr_mask;
        }
    }

    pub fn set_audio_min_period_cck(&mut self, period: u16) {
        self.audio_min_period_cck = period.max(1);
    }

    #[cfg(test)]
    pub fn audio_min_period_cck(&self) -> u16 {
        self.audio_min_period_cck
    }

    #[cfg(test)]
    pub fn set_audio_dma_ptr_for_test(&mut self, ch_idx: usize, ptr: u32) {
        let ptr = ptr & self.dma_ptr_mask();
        if let Some(ch) = self.chans.get_mut(ch_idx) {
            ch.ptr = ptr;
        }
    }

    #[cfg(test)]
    pub fn audio_dma_ptr_for_test(&self, ch_idx: usize) -> Option<u32> {
        self.chans.get(ch_idx).map(|ch| ch.ptr)
    }

    #[cfg(test)]
    pub fn audio_current_sample_for_test(&self, ch_idx: usize) -> Option<i8> {
        self.chans.get(ch_idx).map(|ch| ch.current)
    }

    pub fn reset_registers(&mut self) {
        self.serper = 0;
        self.intena = 0;
        self.intreq = 0;
        self.adkcon = 0;
        self.potgo = 0;
        self.chans = [
            AudChannel::new(),
            AudChannel::new(),
            AudChannel::new(),
            AudChannel::new(),
        ];
        self.serial_tx_buffer = None;
        self.serial_tx_shift = None;
        self.serial_rx_shift = None;
        self.serial_rx_buffer = None;
        self.serial_overrun = false;
        self.serial_rx_pin_high = true;
        self.serial_rx_sync_0_high = true;
        self.serial_rx_sync_1_high = true;
        self.serial_tx_pin_high = true;
        self.pot_counters = [0; 4];
        self.pot_running = false;
        self.pot_acc_cck = 0;
        self.last_dmacon = 0;
        self.host_sample_acc = 0;
        self.led_filter_enabled = true;
        self.led_filter = StereoLedFilter::new();
        self.audio_mod_next_period = [false; 4];
        self.audio_min_period_cck = PAL_AUDIO_MIN_PERIOD_CCK;
    }

    pub fn set_led_filter_enabled(&mut self, enabled: bool) {
        self.led_filter_enabled = enabled;
    }

    pub fn set_output_volume_percent(&mut self, percent: u8) {
        self.output_volume = f32::from(percent.min(100)) / 100.0;
    }

    pub fn drive_sounds_mut(&mut self) -> &mut DriveSounds {
        &mut self.drive_sounds
    }

    pub fn cd_audio_mut(&mut self) -> &mut CdAudioRing {
        &mut self.cd_audio
    }

    pub fn output_volume_percent(&self) -> u8 {
        (self.output_volume * 100.0).round().clamp(0.0, 100.0) as u8
    }

    pub fn live_audio_output_lead_seconds(&self) -> f64 {
        self.audio.live_output_lead_seconds()
    }

    pub fn live_audio_status(&self) -> AudioRuntimeStatus {
        self.audio.runtime_status()
    }

    pub fn set_live_audio_suspended(&mut self, suspended: bool) {
        self.audio.set_live_output_suspended(suspended);
    }

    /// SERDAT write: bits 7..0 are the data byte; bit 8 is either the
    /// ninth data bit or the first stop bit depending on SERPER. The
    /// model keeps a one-word transmit buffer and a timed shift register.
    pub fn write_serdat(&mut self, val: u16) -> u16 {
        self.serial_tx_buffer = Some(val);
        self.load_serial_shift_if_idle()
    }

    /// SERDATR read: bit 13 = TBE (transmit buffer empty), bit 12 =
    /// TSRE (transmit shift register empty), bit 14 = RBF (receive
    /// buffer full), bit 15 = overrun.
    pub fn read_serdatr(&self) -> u16 {
        let mut v = self.serial_rx_buffer.unwrap_or(0);
        if self.serial_overrun {
            v |= 1 << 15;
        }
        if self.rbf_mirror() {
            v |= 1 << 14;
        }
        if self.serial_tx_buffer.is_none() {
            v |= 1 << 13;
        }
        if self.serial_tx_shift.is_none() {
            v |= 1 << 12;
        }
        if self.serial_rx_sync_1_high {
            v |= 1 << 11;
        }
        v
    }

    /// INTENA writes use SET/CLR semantics on bit 15.
    pub fn write_intena(&mut self, val: u16) {
        let bits = val & 0x7FFF;
        if val & 0x8000 != 0 {
            self.intena |= bits;
        } else {
            self.intena &= !bits;
        }
    }

    /// INTREQ writes also use SET/CLR semantics. Returns true if the
    /// write asserted a new bit (used by the bus to preempt the slice
    /// so the freshly-set IRQ delivers before agnus piles on VERTB).
    pub fn write_intreq(&mut self, val: u16) -> bool {
        self.write_intreq_with_source_bits(val, 0)
    }

    pub fn write_intreq_with_source_bits(&mut self, val: u16, source_bits: u16) -> bool {
        let bits = val & INTREQ_MASK;
        let source_bits = source_bits & INTREQ_MASK;
        let before = self.intreq;
        if val & 0x8000 != 0 {
            self.intreq |= bits;
        } else {
            self.intreq &= !bits;
            if bits & INT_RBF != 0 && source_bits & INT_RBF == 0 {
                self.serial_rx_buffer = None;
                self.serial_overrun = false;
            }
        }
        self.intreq |= source_bits;
        (self.intreq & !before) != 0
    }

    pub fn latch_interrupt_sources(&mut self, source_bits: u16) -> bool {
        self.write_intreq_with_source_bits(0, source_bits)
    }

    #[cfg(test)]
    pub fn audpen_bits(&self) -> u8 {
        ((self.intreq >> 7) & 0x0F) as u8
    }

    fn rbf_mirror(&self) -> bool {
        self.intreq & INT_RBF != 0
    }

    /// ADKCON: audio modulation control and disk/serial mode bits.
    pub fn write_adkcon(&mut self, val: u16) {
        let before = self.adkcon;
        let bits = val & 0x7FFF;
        if val & 0x8000 != 0 {
            self.adkcon |= bits;
        } else {
            self.adkcon &= !bits;
        }
        for source in 0..self.audio_mod_next_period.len() {
            let mask = (1 << source) | (1 << (source + 4));
            if before & mask != mask && self.adkcon & mask == mask {
                self.audio_mod_next_period[source] = false;
            }
        }
    }

    fn channel_attached_as_modulator(&self, ch_idx: usize) -> bool {
        let volume_attach = 1u16 << ch_idx;
        let period_attach = 1u16 << (ch_idx + 4);
        self.adkcon & (volume_attach | period_attach) != 0
    }

    pub fn write_potgo(&mut self, val: u16) {
        self.potgo = val & 0xFF01;
        if val & 0x0001 != 0 {
            self.pot_counters = [0; 4];
            self.pot_running = true;
            self.pot_acc_cck = 0;
        }
    }

    pub fn read_potdat(&self, port: usize) -> u16 {
        match port {
            0 => ((self.pot_counters[1] as u16) << 8) | self.pot_counters[0] as u16,
            _ => ((self.pot_counters[3] as u16) << 8) | self.pot_counters[2] as u16,
        }
    }

    pub fn read_potgor(&self, pins: PotPins) -> u16 {
        let mut v = self.potgo & 0xFF00;
        for (bit, released) in [
            (8, pins.left_x_released),
            (10, pins.left_y_released),
            (12, pins.right_x_released),
            (14, pins.right_y_released),
        ] {
            let out_bit = bit + 1;
            let mask = 1u16 << bit;
            // The pot pins are open-drain with a weak pull-up: a connected
            // button is a switch to ground. Driving the pin LOW (output enable
            // + data 0) forces it low, but driving it HIGH (output enable +
            // data 1) is only a pull-up that a pressed button still pulls low.
            // With output disabled the pin floats and likewise reads the
            // button. So the button is visible in every mode except a hard low
            // drive -- this is how software reads fire 2/3 by enabling the
            // pull-up (e.g. AmigaTestKit writes POTGO = 0x0f00 << port*4).
            let driven_low = self.potgo & (1u16 << out_bit) != 0 && self.potgo & mask == 0;
            if driven_low || !released {
                v &= !mask;
            } else {
                v |= mask;
            }
        }
        v
    }

    pub fn tick_pots(&mut self, cck: u32) {
        if !self.pot_running {
            return;
        }
        self.pot_acc_cck = self.pot_acc_cck.saturating_add(cck);
        while self.pot_acc_cck >= POT_COUNTER_CCK {
            self.pot_acc_cck -= POT_COUNTER_CCK;
            for counter in &mut self.pot_counters {
                *counter = counter.saturating_add(1);
            }
            if self.pot_counters.iter().all(|&counter| counter == u8::MAX) {
                self.pot_running = false;
                break;
            }
        }
    }

    pub fn next_pot_event_cck(&self) -> Option<u32> {
        if !self.pot_running {
            return None;
        }
        Some(POT_COUNTER_CCK.saturating_sub(self.pot_acc_cck).max(1))
    }

    pub fn tick_serial(&mut self, cck: u32) -> u16 {
        // Idle fast path: nothing shifting, nothing queued in either
        // direction. Equivalent to the full path, which would only advance
        // the RX synchronizer (the pin level cannot change while idle).
        if self.serial_tx_shift.is_none()
            && self.serial_tx_buffer.is_none()
            && self.serial_rx_shift.is_none()
            && !self.serial.has_pending_input()
        {
            self.advance_serial_rx_synchronizer(cck);
            return 0;
        }
        self.tick_serial_tx(cck) | self.tick_serial_rx(cck)
    }

    fn tick_serial_tx(&mut self, cck: u32) -> u16 {
        let mut irq = 0;
        let mut remaining = cck;
        while remaining > 0 {
            if let Some(mut shift) = self.serial_tx_shift.take() {
                let step = remaining.min(shift.remaining_cck);
                remaining -= step;
                shift.remaining_cck -= step;
                shift.break_seen |= self.uart_break_active();
                if shift.remaining_cck > 0 {
                    self.serial_tx_shift = Some(shift);
                    break;
                }

                shift.bit_index += 1;
                if shift.bit_index >= shift.total_bits {
                    if !shift.break_seen && !self.uart_break_active() {
                        self.serial
                            .write_word(Self::serial_tx_data_word(&shift), shift.long);
                    }
                    self.serial_tx_pin_high = true;
                    irq |= self.load_serial_shift_if_idle();
                } else {
                    shift.remaining_cck = shift.bit_cck;
                    self.serial_tx_pin_high = Self::serial_tx_bit(&shift);
                    self.serial_tx_shift = Some(shift);
                }
            } else {
                irq |= self.load_serial_shift_if_idle();
                if self.serial_tx_shift.is_none() {
                    break;
                }
            }
        }
        irq
    }

    fn tick_serial_rx(&mut self, cck: u32) -> u16 {
        let mut irq = 0;
        let long = self.serial_long();
        self.load_serial_rx_shift_if_idle(long);

        let mut remaining = cck;
        while remaining > 0 {
            let Some(mut shift) = self.serial_rx_shift.take() else {
                self.advance_serial_rx_synchronizer(remaining);
                break;
            };
            let step = remaining.min(shift.remaining_cck);
            remaining -= step;
            shift.remaining_cck -= step;
            self.advance_serial_rx_synchronizer(step);
            if shift.remaining_cck > 0 {
                self.serial_rx_shift = Some(shift);
                break;
            }

            shift.bit_index += 1;
            if shift.bit_index >= shift.total_bits {
                let word = Self::serdatr_receive_word(shift.word, shift.long);
                if self.serial_rx_buffer.is_some() {
                    self.serial_overrun = true;
                } else {
                    self.serial_rx_buffer = Some(word);
                    irq |= INT_RBF;
                }
                self.serial_rx_pin_high = true;
                self.load_serial_rx_shift_if_idle(long);
            } else {
                shift.remaining_cck = shift.bit_cck;
                self.serial_rx_pin_high = Self::serial_rx_bit(&shift);
                self.serial_rx_shift = Some(shift);
            }
        }
        irq
    }

    pub fn next_serial_event_cck(&self) -> Option<u32> {
        let tx = self
            .serial_tx_shift
            .as_ref()
            .map(|shift| shift.remaining_cck.max(1));
        let rx = self
            .serial_rx_shift
            .as_ref()
            .map(|shift| shift.remaining_cck.max(1));
        match (tx, rx) {
            (Some(tx), Some(rx)) => Some(tx.min(rx)),
            (Some(tx), None) => Some(tx),
            (None, Some(rx)) => Some(rx),
            (None, None) => None,
        }
    }

    #[cfg(test)]
    pub fn serial_txd_pin_high(&self) -> bool {
        !self.uart_break_active() && self.serial_tx_pin_high
    }

    pub fn next_audio_irq_cck(&self, dmacon: u16) -> Option<u32> {
        if dmacon & DMACON_DMAEN == 0 {
            return None;
        }
        self.chans
            .iter()
            .enumerate()
            .filter_map(|(idx, ch)| {
                if dmacon & (1u16 << idx) == 0 {
                    return None;
                }
                ch.next_irq_cck()
            })
            .min()
    }

    fn load_serial_shift_if_idle(&mut self) -> u16 {
        if self.serial_tx_shift.is_some() {
            return 0;
        }
        let Some(word) = self.serial_tx_buffer.take() else {
            return 0;
        };
        let long = self.serial_long();
        let bit_cck = self.serial_bit_cck();
        let shift = SerialTxShift {
            word,
            long,
            bit_cck,
            remaining_cck: bit_cck,
            bit_index: 0,
            total_bits: Self::serial_tx_total_bits(word, long),
            break_seen: self.uart_break_active(),
        };
        self.serial_tx_pin_high = Self::serial_tx_bit(&shift);
        self.serial_tx_shift = Some(shift);
        INT_TBE
    }

    fn load_serial_rx_shift_if_idle(&mut self, long: bool) {
        if self.serial_rx_shift.is_some() {
            return;
        }
        let Some(word) = self.serial.read_word(long) else {
            return;
        };
        let bit_cck = self.serial_bit_cck();
        let shift = SerialRxShift {
            word,
            long,
            bit_cck,
            remaining_cck: bit_cck,
            bit_index: 0,
            total_bits: if long { 11 } else { 10 },
        };
        self.serial_rx_pin_high = Self::serial_rx_bit(&shift);
        self.serial_rx_shift = Some(shift);
    }

    fn serial_long(&self) -> bool {
        self.serper & SERPER_LONG != 0
    }

    fn serial_bit_cck(&self) -> u32 {
        u32::from(self.serper & 0x7FFF).saturating_add(1).max(1)
    }

    fn uart_break_active(&self) -> bool {
        self.adkcon & ADKCON_UARTBRK != 0
    }

    fn serial_tx_total_bits(word: u16, long: bool) -> u8 {
        if word == 0 {
            return if long { 11 } else { 10 };
        }
        let highest = u16::BITS as u8 - 1 - word.leading_zeros() as u8;
        1 + highest + 1
    }

    fn serial_tx_bit(shift: &SerialTxShift) -> bool {
        if shift.bit_index == 0 {
            false
        } else {
            shift.word & (1u16 << (shift.bit_index - 1)) != 0
        }
    }

    fn serial_tx_data_word(shift: &SerialTxShift) -> u16 {
        if shift.long {
            shift.word & 0x01FF
        } else {
            shift.word & 0x00FF
        }
    }

    fn serial_rx_bit(shift: &SerialRxShift) -> bool {
        if shift.bit_index == 0 {
            false
        } else {
            let data_bits = if shift.long { 9 } else { 8 };
            if shift.bit_index <= data_bits {
                shift.word & (1u16 << (shift.bit_index - 1)) != 0
            } else {
                true
            }
        }
    }

    fn advance_serial_rx_synchronizer(&mut self, cck: u32) {
        for _ in 0..cck.min(2) {
            self.serial_rx_sync_1_high = self.serial_rx_sync_0_high;
            self.serial_rx_sync_0_high = self.serial_rx_pin_high;
        }
    }

    fn serdatr_receive_word(word: u16, long: bool) -> u16 {
        if long {
            (word & 0x01FF) | 0x0200
        } else {
            (word & 0x00FF) | 0x0300
        }
    }

    /// Audio register write. `reg_off` is the offset within the
    /// $DFF0A0..$DFF0DF audio block (i.e. addr - $DFF0A0). Each
    /// channel occupies 16 bytes; the per-channel layout is:
    /// `+0 LCH +2 LCL +4 LEN +6 PER +8 VOL +A DAT`.
    pub fn write_audio_reg(&mut self, reg_off: u16, val: u16) {
        let ch_idx = (reg_off / 0x10) as usize;
        if ch_idx >= 4 {
            return;
        }
        let ptr_mask = self.dma_ptr_mask();
        let ch = &mut self.chans[ch_idx];
        match reg_off & 0x0F {
            0x0 => {
                // AUDxLCH: high 5 bits of chip-RAM address.
                ch.lc = ((ch.lc & 0x0000_FFFF) | (((val as u32) & 0x001F) << 16)) & ptr_mask;
            }
            0x2 => {
                // AUDxLCL: low 15 bits, low bit cleared (word-aligned).
                ch.lc = ((ch.lc & 0xFFFF_0000) | ((val as u32) & 0xFFFE)) & ptr_mask;
            }
            0x4 => {
                ch.len = val;
            }
            0x6 => {
                ch.per = val;
            }
            0x8 => {
                // AUDxVOL bits 0..6, max 64.
                ch.vol = paula_volume_from_word(val);
            }
            0xA => {
                // AUDxDAT: CPU-driven sample data. The next audio tick
                // starts or restarts manual playback if DMA is disabled.
                // Keep this separate from the live DMA word buffer so
                // CPU writes cannot corrupt an active DMA stream.
                ch.dat_latch = val;
                ch.manual_pending = true;
            }
            _ => {}
        }
    }

    /// Audio register read. AUDxDAT reads return 0 on real hardware
    /// (it's write-only). We return 0 for everything in this block.
    pub fn read_audio_reg(&self, _reg_off: u16) -> u16 {
        0
    }

    #[cfg(test)]
    pub fn peek_audio_reg_latch(&self, reg_off: u16) -> Option<u16> {
        let ch_idx = (reg_off / 0x10) as usize;
        if ch_idx >= 4 {
            return None;
        }
        let ch = &self.chans[ch_idx];
        match reg_off & 0x0F {
            0x0 => Some(((ch.lc >> 16) & 0x001F) as u16),
            0x2 => Some((ch.lc & 0xFFFE) as u16),
            0x4 => Some(ch.len),
            0x6 => Some(ch.per),
            0x8 => Some(ch.vol as u16),
            0xA => Some(ch.dat_latch),
            _ => None,
        }
    }

    fn dma_ptr_mask(&self) -> u32 {
        self.dma_addr_mask & !1
    }

    /// Advance Paula's audio state by `cck` color clocks and emit
    /// interleaved stereo frames to the AudioSink. Audio DMA memory
    /// words are supplied separately through `grant_audio_dma`, which
    /// lets Agnus own the documented channel slots.
    pub fn advance_audio(&mut self, cck: u32, dmacon: u16) -> u16 {
        let mut irq_bits = 0;
        let master = dmacon & DMACON_DMAEN != 0;
        let ptr_mask = self.dma_ptr_mask();

        // The edge-detection scan below only acts on a DMACON change or a
        // pending CPU AUDxDAT write (manual_pending); with neither, every
        // branch is a no-op (enable edges impossible, manual latching idle,
        // disabled channels already parked Off by the previous scan). This
        // runs per chip-bus quantum, so skip the 4-channel walk when idle.
        if dmacon != self.last_dmacon || self.chans.iter().any(|ch| ch.manual_pending) {
            self.scan_audio_dma_edges(dmacon, master, ptr_mask, &mut irq_bits);
        }
        self.last_dmacon = dmacon;

        let mut remaining = cck;
        while remaining > 0 {
            let step = remaining.min(self.cck_until_next_output_frame());
            irq_bits |= self.advance_audio_channels(step);
            self.host_sample_acc += step as u64 * MIX_SAMPLE_RATE as u64;
            remaining -= step;

            while self.host_sample_acc >= PAULA_CLOCK_HZ as u64 {
                self.host_sample_acc -= PAULA_CLOCK_HZ as u64;
                self.push_mixed_frame();
            }
        }

        irq_bits
    }

    /// Per-channel DMA-enable edge detection and state machine; see the
    /// idle skip in `advance_audio` for when this may be elided.
    fn scan_audio_dma_edges(
        &mut self,
        dmacon: u16,
        master: bool,
        ptr_mask: u32,
        irq_bits: &mut u16,
    ) {
        for ch_idx in 0..4 {
            let enabled = master && (dmacon & (1 << ch_idx)) != 0;
            let prev_enabled =
                (self.last_dmacon & DMACON_DMAEN != 0) && (self.last_dmacon & (1 << ch_idx)) != 0;
            let ch = &mut self.chans[ch_idx];
            if enabled && !prev_enabled {
                // DMA-enable rising edge: latch a fresh buffer from
                // AUDxLC/AUDxLEN and raise the per-channel IRQ. Real
                // Paula uses this to let the CPU prime the *next*
                // buffer. The first word is fetched into the holding
                // register and promoted to start playback.
                ch.reload_buffer(ptr_mask);
                ch.reset_dma_start_timing();
                ch.state = ChanState::StartPending;
                ch.manual_pending = false;
                ch.request_dma();
                *irq_bits |= INT_AUDX[ch_idx];
            } else if enabled {
                // CPU AUDxDAT writes made while DMA owns the channel
                // update the CPU-visible data latch but must not arm a
                // stale manual restart or replace the DMA sample word.
                ch.manual_pending = false;
            } else if !enabled {
                if ch.manual_pending {
                    if self.intreq & INT_AUDX[ch_idx] == 0 {
                        ch.latch_manual_word();
                    } else {
                        ch.manual_pending = false;
                    }
                } else if !matches!(ch.state, ChanState::Manual | ChanState::ManualHold) {
                    ch.state = ChanState::Off;
                    ch.current = 0;
                    ch.next_word = None;
                    ch.restart_pending = false;
                    ch.clear_dma_request();
                }
            }
        }
    }

    #[cfg(test)]
    pub fn tick_audio(&mut self, cck: u32, dmacon: u16, chip_ram: &[u8]) -> u16 {
        let mut irq_bits = 0;
        for _ in 0..cck {
            irq_bits |= self.advance_audio(0, dmacon);
            irq_bits |= self.grant_test_audio_dma(chip_ram);
            irq_bits |= self.advance_audio(1, dmacon);
            irq_bits |= self.grant_test_audio_dma(chip_ram);
        }
        irq_bits
    }

    #[cfg(test)]
    fn grant_test_audio_dma(&mut self, chip_ram: &[u8]) -> u16 {
        let mut irq_bits = 0;
        for ch_idx in 0..self.chans.len() {
            let mut grants = 0;
            while let Some(request) = self.audio_dma_request(ch_idx) {
                let word = read_chip_word_for_audio_test(chip_ram, request.address);
                irq_bits |= self.grant_audio_dma(ch_idx, word);
                grants += 1;
                debug_assert!(grants <= 2);
            }
        }
        irq_bits
    }

    fn cck_until_next_output_frame(&self) -> u32 {
        let needed = (PAULA_CLOCK_HZ as u64).saturating_sub(self.host_sample_acc);
        needed.div_ceil(MIX_SAMPLE_RATE as u64).max(1) as u32
    }

    pub fn audio_dma_request(&self, ch_idx: usize) -> Option<AudioDmaRequest> {
        let ch = self.chans.get(ch_idx)?;
        if !ch.dma_request {
            return None;
        }
        Some(AudioDmaRequest {
            address: ch.ptr & self.dma_ptr_mask(),
        })
    }

    pub fn grant_audio_dma(&mut self, ch_idx: usize, word: u16) -> u16 {
        let ptr_mask = self.dma_ptr_mask();
        let min_period = u32::from(self.audio_min_period_cck);
        let Some(ch) = self.chans.get_mut(ch_idx) else {
            return 0;
        };
        if !ch.dma_request {
            return 0;
        }

        // Read the word into the fetch-ahead holding register and step
        // the pointer/length counter. This is the channel's per-line
        // DMA slot doing its read one word ahead of the output shifter.
        ch.clear_dma_request();
        let buffer_wrapped = ch.accept_dma_word(word, ptr_mask);
        ch.dma_fetch_cooldown_cck = min_period;
        let mut irq_bits = 0;
        if buffer_wrapped {
            irq_bits |= INT_AUDX[ch_idx];
        }

        // The very first fetch after a DMA-enable starts playback right
        // away: promote the held word into the active buffer so the
        // output shifter has data, and arm the prefetch of the next
        // word for the holding register.
        if ch.state == ChanState::StartPending {
            ch.promote_next_word();
            ch.current = ch.word_hi;
            ch.phase = 1;
            ch.period_acc = 0;
            ch.state = ChanState::Running;
            let loaded_word = ((ch.word_hi as u8 as u16) << 8) | ch.word_lo as u8 as u16;
            self.apply_audio_modulation(&[AudioModEvent {
                source: ch_idx,
                word: loaded_word,
                word_start: true,
            }]);
        }
        irq_bits
    }

    fn advance_audio_channels(&mut self, cck: u32) -> u16 {
        let mut irq_bits = 0;
        let mut mod_events = Vec::new();
        let ptr_mask = self.dma_ptr_mask();
        for ch_idx in 0..4 {
            let ch = &mut self.chans[ch_idx];
            if matches!(ch.state, ChanState::Off | ChanState::ManualHold) {
                continue;
            }
            if ch.state == ChanState::Manual {
                if ch.per == 0 {
                    continue;
                }
                ch.period_acc += cck;
                while ch.period_acc >= ch.per as u32 {
                    ch.period_acc -= ch.per as u32;
                    if ch.phase == 0 {
                        ch.current = ch.word_hi;
                        ch.phase = 1;
                    } else {
                        ch.current = ch.word_lo;
                        ch.phase = 0;
                        irq_bits |= INT_AUDX[ch_idx];
                        ch.state = ChanState::ManualHold;
                        break;
                    }
                }
                continue;
            }
            if ch.state == ChanState::StartPending {
                if !ch.dma_request {
                    ch.request_dma();
                }
                continue;
            }
            if ch.state == ChanState::Running {
                // AUDxPER=0 is outside the documented range. Keep the
                // current sample stable rather than letting the period
                // accumulator underflow.
                if ch.per == 0 {
                    continue;
                }
                let period = ch.per as u32;
                let mut channel_remaining = cck;
                while channel_remaining > 0 {
                    let until_period = period.saturating_sub(ch.period_acc).max(1);
                    let until_dma_ready = ch.dma_fetch_cooldown_cck.max(1);
                    let step = if ch.dma_fetch_cooldown_cck == 0 {
                        channel_remaining.min(until_period)
                    } else {
                        channel_remaining.min(until_period.min(until_dma_ready))
                    };
                    ch.period_acc += step;
                    ch.dma_fetch_cooldown_cck = ch.dma_fetch_cooldown_cck.saturating_sub(step);
                    channel_remaining -= step;
                    // Keep the fetch-ahead holding register topped up. As
                    // soon as it is empty and the per-line fetch budget has
                    // recovered, ask Agnus for the next word so it arrives
                    // before the active word is exhausted.
                    if ch.next_word.is_none() && ch.dma_fetch_cooldown_cck == 0 && !ch.dma_request {
                        ch.arm_next_buffer_fetch(ptr_mask);
                    }
                    if ch.period_acc < period {
                        continue;
                    }
                    ch.period_acc -= period;
                    // Emit the next byte and advance phase.
                    if ch.phase == 0 {
                        ch.current = ch.word_hi;
                        mod_events.push(AudioModEvent {
                            source: ch_idx,
                            word: ((ch.word_hi as u8 as u16) << 8) | ch.word_lo as u8 as u16,
                            word_start: true,
                        });
                        ch.phase = 1;
                    } else {
                        ch.current = ch.word_lo;
                        mod_events.push(AudioModEvent {
                            source: ch_idx,
                            word: ((ch.word_hi as u8 as u16) << 8) | ch.word_lo as u8 as u16,
                            word_start: false,
                        });
                        ch.phase = 0;
                        // Whole 16-bit word consumed: promote the word the
                        // DMA slot fetched ahead into the holding register.
                        // If it has not arrived yet (DMA could not keep up,
                        // e.g. AUDxPER below the per-line fetch budget) the
                        // active word's two samples simply repeat.
                        ch.promote_next_word();
                        if ch.next_word.is_none()
                            && ch.dma_fetch_cooldown_cck == 0
                            && !ch.dma_request
                        {
                            ch.arm_next_buffer_fetch(ptr_mask);
                        }
                    }
                }
            }
        }
        self.apply_audio_modulation(&mod_events);
        irq_bits
    }

    fn apply_audio_modulation(&mut self, events: &[AudioModEvent]) {
        for event in events {
            let target = event.source + 1;
            if target >= self.chans.len() {
                continue;
            }
            let volume_attach = self.adkcon & (1 << event.source) != 0;
            let period_attach = self.adkcon & (1 << (event.source + 4)) != 0;
            if volume_attach && period_attach {
                if !event.word_start {
                    continue;
                }
                if self.audio_mod_next_period[event.source] {
                    self.chans[target].per = event.word.max(1);
                } else {
                    self.chans[target].vol = paula_volume_from_word(event.word);
                }
                self.audio_mod_next_period[event.source] =
                    !self.audio_mod_next_period[event.source];
                continue;
            }
            if volume_attach {
                self.chans[target].vol = paula_volume_from_word(event.word);
            }
            if period_attach {
                self.chans[target].per = event.word.max(1);
            }
        }
    }

    fn push_mixed_frame(&mut self) {
        // Mix and push host-rate stereo frames into the sink. Paula
        // stereo routing follows the common A500/A600/A1200 and
        // Minimig mapping: channels 0 and 3 go left, 1 and 2 right.
        // Some HRM prose and motherboard jack labels describe the
        // opposite physical side; keep this as channel-to-DAC routing.
        // Minimig also exposes PWM-gated per-channel samples, but its
        // mixed DAC output uses the linear volume multiplier. Keep the
        // host PCM path linear until modelling the alternate PWM/filter
        // path as an explicit analog output mode.
        // Volume range is 0..64; each channel sample is signed 8-bit
        // (-128..127). Scale into [-1.0, 1.0] approximately by
        // dividing by (128.0 * 64.0). We sum two channels per side
        // unclipped: worst case is +/-2.0 if both channels saturate
        // with full volume in opposite phase, which is essentially
        // never the case for real music.
        let l_raw = self.channel_mixed_sample(0) + self.channel_mixed_sample(3);
        let r_raw = self.channel_mixed_sample(1) + self.channel_mixed_sample(2);
        let scale = 1.0 / (128.0 * 64.0);
        let mut left = l_raw as f32 * scale;
        let mut right = r_raw as f32 * scale;
        let filtered = self.led_filter.process(left, right);
        if self.led_filter_enabled {
            (left, right) = filtered;
        }
        // Drive noises join after the LED filter (acoustic, not part of
        // Paula's output path) but under the master volume control.
        let drive = self.drive_sounds.next_sample();
        left += drive;
        right += drive;
        // CD audio (CD32/CDTV) is line-mixed with Paula's output after
        // the LED filter, like the real mixer stage, and also sits under
        // the master volume control.
        let (cd_left, cd_right) = self.cd_audio.next_sample();
        left += cd_left;
        right += cd_right;
        if let Some(capture) = &mut self.capture {
            capture.push((left, right));
        }
        self.audio
            .push(left * self.output_volume, right * self.output_volume);
    }

    fn channel_mixed_sample(&self, ch_idx: usize) -> i32 {
        if self.channel_attached_as_modulator(ch_idx) {
            0
        } else {
            let ch = &self.chans[ch_idx];
            (ch.current as i32) * (ch.vol as i32)
        }
    }
}

#[derive(Clone, Copy, serde::Serialize, serde::Deserialize)]
struct StereoLedFilter {
    left: AnalogLedFilter,
    right: AnalogLedFilter,
}

impl StereoLedFilter {
    fn new() -> Self {
        Self {
            left: AnalogLedFilter::new(LED_FILTER_CUTOFF_HZ, MIX_SAMPLE_RATE as f32),
            right: AnalogLedFilter::new(LED_FILTER_CUTOFF_HZ, MIX_SAMPLE_RATE as f32),
        }
    }

    fn process(&mut self, left: f32, right: f32) -> (f32, f32) {
        (self.left.process(left), self.right.process(right))
    }
}

#[derive(Clone, Copy, serde::Serialize, serde::Deserialize)]
struct AnalogLedFilter {
    one_pole: OnePoleLowPass,
    two_pole: BiquadLowPass,
}

impl AnalogLedFilter {
    fn new(cutoff_hz: f32, sample_rate_hz: f32) -> Self {
        Self {
            one_pole: OnePoleLowPass::new(cutoff_hz, sample_rate_hz),
            two_pole: BiquadLowPass::new(cutoff_hz, sample_rate_hz),
        }
    }

    fn process(&mut self, input: f32) -> f32 {
        self.two_pole.process(self.one_pole.process(input))
    }
}

#[derive(Clone, Copy, serde::Serialize, serde::Deserialize)]
struct OnePoleLowPass {
    alpha: f32,
    z: f32,
}

impl OnePoleLowPass {
    fn new(cutoff_hz: f32, sample_rate_hz: f32) -> Self {
        let alpha = 1.0 - (-2.0 * PI * cutoff_hz / sample_rate_hz).exp();
        Self { alpha, z: 0.0 }
    }

    fn process(&mut self, input: f32) -> f32 {
        self.z += self.alpha * (input - self.z);
        self.z
    }
}

#[derive(Clone, Copy, serde::Serialize, serde::Deserialize)]
struct BiquadLowPass {
    b0: f32,
    b1: f32,
    b2: f32,
    a1: f32,
    a2: f32,
    z1: f32,
    z2: f32,
}

impl BiquadLowPass {
    fn new(cutoff_hz: f32, sample_rate_hz: f32) -> Self {
        let omega = 2.0 * PI * cutoff_hz / sample_rate_hz;
        let sin = omega.sin();
        let cos = omega.cos();
        let q = std::f32::consts::FRAC_1_SQRT_2;
        let alpha = sin / (2.0 * q);

        let b0 = (1.0 - cos) * 0.5;
        let b1 = 1.0 - cos;
        let b2 = b0;
        let a0 = 1.0 + alpha;
        let a1 = -2.0 * cos;
        let a2 = 1.0 - alpha;

        Self {
            b0: b0 / a0,
            b1: b1 / a0,
            b2: b2 / a0,
            a1: a1 / a0,
            a2: a2 / a0,
            z1: 0.0,
            z2: 0.0,
        }
    }

    fn process(&mut self, input: f32) -> f32 {
        let output = self.b0 * input + self.z1;
        self.z1 = self.b1 * input - self.a1 * output + self.z2;
        self.z2 = self.b2 * input - self.a2 * output;
        output
    }
}

impl Drop for Paula {
    fn drop(&mut self) {
        self.audio.flush();
        self.serial.flush();
    }
}

/// Map a set of pending+enabled Paula interrupt bits to a 68K IPL.
/// Returns 0 if nothing is pending. The mapping is fixed by the
/// hardware: the chipset wires each interrupt line to a specific CPU
/// IPL level (see Amiga Hardware Reference Manual, Paula chapter).
pub fn pending_ipl(pending: u16) -> u8 {
    // EXTER, plus the undocumented INT14 source which shares EXTER's
    // IPL in the Paula RTL.
    if pending & (INT_INT14 | (1 << 13)) != 0 {
        6
    }
    // DSKSYN, RBF
    else if pending & ((1 << 12) | (1 << 11)) != 0 {
        5
    }
    // AUD0..AUD3 (bits 7..10)
    else if pending & 0x0780 != 0 {
        4
    }
    // BLIT, VERTB, COPER
    else if pending & ((1 << 6) | (1 << 5) | (1 << 4)) != 0 {
        3
    }
    // PORTS
    else if pending & (1 << 3) != 0 {
        2
    }
    // SOFT, DSKBLK, TBE
    else if pending & ((1 << 2) | (1 << 1) | (1 << 0)) != 0 {
        1
    } else {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::WavSink;
    use std::cell::RefCell;
    use std::collections::BTreeSet;
    use std::process;
    use std::rc::Rc;
    use std::sync::{Arc, Mutex};

    struct NoopSerial;

    impl SerialSink for NoopSerial {
        fn write_byte(&mut self, _b: u8) {}
        fn flush(&mut self) {}
    }

    struct CollectSerial {
        written: Arc<Mutex<Vec<u8>>>,
        read: Arc<Mutex<Vec<u8>>>,
    }

    impl SerialSink for CollectSerial {
        fn write_byte(&mut self, b: u8) {
            self.written.lock().unwrap().push(b);
        }

        fn read_byte(&mut self) -> Option<u8> {
            let mut read = self.read.lock().unwrap();
            if read.is_empty() {
                None
            } else {
                Some(read.remove(0))
            }
        }

        fn has_pending_input(&self) -> bool {
            !self.read.lock().unwrap().is_empty()
        }

        fn flush(&mut self) {}
    }

    struct CollectSerialWords {
        written: Arc<Mutex<Vec<(u16, bool)>>>,
        read: Arc<Mutex<Vec<u16>>>,
    }

    impl SerialSink for CollectSerialWords {
        fn write_byte(&mut self, b: u8) {
            self.written.lock().unwrap().push((u16::from(b), false));
        }

        fn write_word(&mut self, word: u16, long: bool) {
            self.written.lock().unwrap().push((word, long));
        }

        fn read_word(&mut self, _long: bool) -> Option<u16> {
            let mut read = self.read.lock().unwrap();
            if read.is_empty() {
                None
            } else {
                Some(read.remove(0))
            }
        }

        fn has_pending_input(&self) -> bool {
            !self.read.lock().unwrap().is_empty()
        }

        fn flush(&mut self) {}
    }

    type WordSerialFixture = (Paula, Arc<Mutex<Vec<(u16, bool)>>>, Arc<Mutex<Vec<u16>>>);

    struct CollectSink {
        frames: Rc<RefCell<Vec<(f32, f32)>>>,
    }

    impl AudioSink for CollectSink {
        fn push(&mut self, left: f32, right: f32) {
            self.frames.borrow_mut().push((left, right));
        }

        fn flush(&mut self) {}
    }

    type SharedFrames = Rc<RefCell<Vec<(f32, f32)>>>;

    fn paula_with_collect_sink() -> (Paula, SharedFrames) {
        let frames = Rc::new(RefCell::new(Vec::new()));
        let audio = CollectSink {
            frames: Rc::clone(&frames),
        };
        (Paula::new(Box::new(NoopSerial), Box::new(audio)), frames)
    }

    type SharedBytes = Arc<Mutex<Vec<u8>>>;

    fn paula_with_collect_serial() -> (Paula, SharedBytes, SharedBytes) {
        let written = Arc::new(Mutex::new(Vec::new()));
        let read = Arc::new(Mutex::new(Vec::new()));
        let serial = CollectSerial {
            written: Arc::clone(&written),
            read: Arc::clone(&read),
        };
        (
            Paula::new(Box::new(serial), Box::new(NullAudio)),
            written,
            read,
        )
    }

    fn paula_with_collect_serial_words() -> WordSerialFixture {
        let written = Arc::new(Mutex::new(Vec::new()));
        let read = Arc::new(Mutex::new(Vec::new()));
        let serial = CollectSerialWords {
            written: Arc::clone(&written),
            read: Arc::clone(&read),
        };
        (
            Paula::new(Box::new(serial), Box::new(NullAudio)),
            written,
            read,
        )
    }

    struct NullAudio;

    impl AudioSink for NullAudio {
        fn push(&mut self, _left: f32, _right: f32) {}
        fn flush(&mut self) {}
    }

    #[test]
    fn audio_capture_tap_mirrors_sink_frames_before_master_volume() {
        let (mut paula, frames) = paula_with_collect_sink();
        paula.set_led_filter_enabled(false);
        paula.set_output_volume_percent(50);
        let mut ram = vec![0u8; 512 * 1024];
        ram[0] = 0x7F;
        ram[1] = 0x81;

        paula.write_audio_reg(0x00, 0);
        paula.write_audio_reg(0x02, 0);
        paula.write_audio_reg(0x04, 1);
        paula.write_audio_reg(0x06, 80);
        paula.write_audio_reg(0x08, 64);

        // Tap disabled: nothing accumulates.
        paula.tick_audio(400, DMACON_DMAEN | 0x0001, &ram);
        assert!(paula.take_captured_audio().is_empty());

        paula.set_audio_capture_enabled(true);
        paula.tick_audio(400, DMACON_DMAEN | 0x0001, &ram);
        let captured = paula.take_captured_audio();
        let sink_frames = frames.borrow();
        assert!(!captured.is_empty());
        // The tap carries the same mixed frames as the sink, but before
        // the master output volume (sink got 50%).
        let tail = &sink_frames[sink_frames.len() - captured.len()..];
        for ((cap_l, cap_r), (sink_l, sink_r)) in captured.iter().zip(tail) {
            assert!((cap_l * 0.5 - sink_l).abs() < 1e-6);
            assert!((cap_r * 0.5 - sink_r).abs() < 1e-6);
        }
        drop(sink_frames);

        // Draining empties the buffer; disabling discards new frames.
        assert!(paula.take_captured_audio().is_empty());
        paula.set_audio_capture_enabled(false);
        paula.tick_audio(400, DMACON_DMAEN | 0x0001, &ram);
        assert!(paula.take_captured_audio().is_empty());
    }

    #[test]
    fn large_audio_tick_emits_chronological_samples() {
        let (mut paula, frames) = paula_with_collect_sink();
        paula.set_led_filter_enabled(false);
        let mut ram = vec![0u8; 512 * 1024];
        ram[0] = 0x7F;
        ram[1] = 0x81;
        ram[2] = 0x7F;
        ram[3] = 0x81;

        paula.write_audio_reg(0x00, 0);
        paula.write_audio_reg(0x02, 0);
        paula.write_audio_reg(0x04, 2);
        paula.write_audio_reg(0x06, 80);
        paula.write_audio_reg(0x08, 64);

        paula.tick_audio(400, DMACON_DMAEN | 0x0001, &ram);

        let frames = frames.borrow();
        assert!(
            frames.len() >= 4,
            "expected several output frames, got {}",
            frames.len()
        );
        let unique_left: BTreeSet<i32> = frames
            .iter()
            .map(|(left, _)| (left * 10_000.0).round() as i32)
            .collect();
        assert!(
            unique_left.len() > 1,
            "output frames should reflect byte changes inside the tick: {frames:?}"
        );
    }

    #[test]
    fn audio_irq_deadline_tracks_dma_buffer_reload() {
        let (mut paula, _) = paula_with_collect_sink();
        paula.set_audio_min_period_cck(1);
        let mut ram = vec![0u8; 64];
        ram[0] = 0x11;
        ram[1] = 0x22;
        ram[2] = 0x33;
        ram[3] = 0x44;
        let dmacon = DMACON_DMAEN | 0x0001;

        paula.write_audio_reg(0x00, 0);
        paula.write_audio_reg(0x02, 0);
        paula.write_audio_reg(0x04, 2);
        paula.write_audio_reg(0x06, 10);
        paula.write_audio_reg(0x08, 64);

        assert_eq!(paula.next_audio_irq_cck(dmacon), None);
        assert_eq!(paula.tick_audio(1, dmacon, &ram) & INT_AUD0, INT_AUD0);
        assert_eq!(paula.next_audio_irq_cck(dmacon), Some(29));

        assert_eq!(paula.tick_audio(28, dmacon, &ram) & INT_AUD0, 0);
        assert_eq!(paula.next_audio_irq_cck(dmacon), Some(1));
        assert_eq!(paula.tick_audio(1, dmacon, &ram) & INT_AUD0, INT_AUD0);
    }

    #[test]
    fn cpu_auddat_with_dma_disabled_outputs_high_then_low_byte() {
        let (mut paula, _) = paula_with_collect_sink();
        let ram = vec![0u8; 64];

        paula.write_audio_reg(0x06, 4);
        paula.write_audio_reg(0x08, 64);
        paula.write_audio_reg(0x0A, 0x4080);

        assert_eq!(paula.tick_audio(1, 0, &ram) & INT_AUD0, 0);
        assert_eq!(paula.chans[0].current, 0x40);
        assert_eq!(paula.chans[0].phase, 1);

        assert_eq!(paula.tick_audio(2, 0, &ram) & INT_AUD0, 0);
        assert_eq!(paula.chans[0].current, 0x40);

        assert_eq!(paula.tick_audio(1, 0, &ram) & INT_AUD0, INT_AUD0);
        assert_eq!(paula.chans[0].current, -128);
        assert_eq!(paula.chans[0].state, ChanState::ManualHold);
        assert_eq!(paula.tick_audio(16, 0, &ram) & INT_AUD0, 0);
        assert_eq!(paula.chans[0].current, -128);
    }

    #[test]
    fn cpu_auddat_write_restarts_manual_period_countdown() {
        let (mut paula, _) = paula_with_collect_sink();
        let ram = vec![0u8; 64];

        paula.write_audio_reg(0x06, 8);
        paula.write_audio_reg(0x0A, 0x1020);
        assert_eq!(paula.tick_audio(3, 0, &ram) & INT_AUD0, 0);
        assert_eq!(paula.chans[0].current, 0x10);
        assert_eq!(paula.chans[0].period_acc, 3);

        paula.write_audio_reg(0x0A, 0x3040);
        assert_eq!(paula.tick_audio(1, 0, &ram) & INT_AUD0, 0);
        assert_eq!(paula.chans[0].current, 0x30);
        assert_eq!(paula.chans[0].period_acc, 1);

        assert_eq!(paula.tick_audio(6, 0, &ram) & INT_AUD0, 0);
        assert_eq!(paula.chans[0].current, 0x30);
        assert_eq!(paula.tick_audio(1, 0, &ram) & INT_AUD0, INT_AUD0);
        assert_eq!(paula.chans[0].current, 0x40);
    }

    #[test]
    fn cpu_auddat_write_waits_for_audio_interrupt_clear_in_manual_mode() {
        let (mut paula, _) = paula_with_collect_sink();
        let ram = vec![0u8; 64];

        paula.write_audio_reg(0x06, 4);
        paula.write_audio_reg(0x0A, 0x1020);
        let irq = paula.tick_audio(4, 0, &ram);
        assert_eq!(irq & INT_AUD0, INT_AUD0);
        paula.latch_interrupt_sources(irq);
        assert_eq!(paula.chans[0].state, ChanState::ManualHold);
        assert_eq!(paula.chans[0].current, 0x20);

        paula.write_audio_reg(0x0A, 0x3040);
        assert_eq!(paula.tick_audio(1, 0, &ram) & INT_AUD0, 0);
        assert_eq!(paula.chans[0].state, ChanState::ManualHold);
        assert_eq!(paula.chans[0].current, 0x20);

        assert!(!paula.write_intreq(INT_AUD0));
        paula.write_audio_reg(0x0A, 0x3040);
        assert_eq!(paula.tick_audio(1, 0, &ram) & INT_AUD0, 0);
        assert_eq!(paula.chans[0].state, ChanState::Manual);
        assert_eq!(paula.chans[0].current, 0x30);
    }

    #[test]
    fn audio_dma_length_one_reloads_latched_location_and_length_at_sample_end() {
        let (mut paula, _) = paula_with_collect_sink();
        paula.set_audio_min_period_cck(1);
        let mut ram = vec![0u8; 64];
        ram[0] = 0x11;
        ram[1] = 0x22;
        ram[4] = 0x33;
        ram[5] = 0x44;
        ram[6] = 0x55;
        ram[7] = 0x66;
        let dmacon = DMACON_DMAEN | 0x0001;

        paula.write_audio_reg(0x00, 0);
        paula.write_audio_reg(0x02, 0);
        paula.write_audio_reg(0x04, 1);
        paula.write_audio_reg(0x06, 4);
        paula.write_audio_reg(0x08, 64);

        // A one-word buffer wraps its length counter on every fetch and
        // keeps looping at the latched AUDxLC (0), playing its word
        // audibly. The pointer/length reload is deferred one fetch, so the
        // counter reads 0 with a restart pending between fetches.
        assert_eq!(paula.tick_audio(1, dmacon, &ram) & INT_AUD0, INT_AUD0);
        assert_eq!(paula.chans[0].current, 0x11);
        assert_eq!(paula.chans[0].words_left, 0);
        assert!(paula.chans[0].restart_pending);

        // The audio interrupt handler repoints the channel at a longer
        // buffer.
        paula.write_audio_reg(0x02, 4);
        paula.write_audio_reg(0x04, 2);

        assert_eq!(paula.tick_audio(2, dmacon, &ram) & INT_AUD0, 0);
        assert_eq!(paula.chans[0].current, 0x11);
        // A subsequent wrapping fetch reloads the pointer/length from the
        // freshly latched AUDxLC=4/AUDxLEN=2, and the new buffer's word
        // eventually reaches the output shifter
        // (the fetch-ahead pipeline carries already-fetched words, so it
        // arrives a couple of words later).
        let mut guard = 0;
        while paula.chans[0].word_hi != 0x33 {
            paula.tick_audio(1, dmacon, &ram);
            guard += 1;
            assert!(guard <= 24, "new buffer word should play after reload");
        }
        assert_eq!(paula.chans[0].word_lo, 0x44);
    }

    #[test]
    fn audio_dma_length_zero_and_one_play_audibly() {
        // Real Paula has no length-based muting: AUDxLEN=0 latches a
        // 65536-word block (the counter wraps) and AUDxLEN=1 loops a
        // single word, both playing their fetched data. The CD32 boot
        // jingle is a LEN=0 one-shot; muting it left the boot silent.
        for len in [0u16, 1] {
            let (mut paula, _) = paula_with_collect_sink();
            let mut ram = vec![0u8; 64];
            ram[0] = 0x7F;
            ram[1] = 0x81;
            let dmacon = DMACON_DMAEN | 0x0001;

            paula.write_audio_reg(0x00, 0);
            paula.write_audio_reg(0x02, 0);
            paula.write_audio_reg(0x04, len);
            paula.write_audio_reg(0x06, 4);
            paula.write_audio_reg(0x08, 64);

            assert_eq!(paula.tick_audio(1, dmacon, &ram) & INT_AUD0, INT_AUD0);
            assert_eq!(paula.chans[0].word_hi, 0x7F);
            assert_eq!(paula.chans[0].word_lo, 0x81u8 as i8);
            assert_eq!(paula.chans[0].current, 0x7F);
            if len == 0 {
                assert_eq!(paula.chans[0].words_left, 0xFFFF);
                assert!(!paula.chans[0].restart_pending);
            } else {
                assert_eq!(paula.chans[0].words_left, 0);
                assert!(paula.chans[0].restart_pending);
            }

            // One period later the low byte plays -- still audible data,
            // not a muted zero.
            assert_eq!(paula.tick_audio(3, dmacon, &ram) & INT_AUD0, 0);
            assert_eq!(paula.chans[0].current, 0x81u8 as i8);
        }
    }

    #[test]
    fn audio_dma_start_fetches_first_word_from_latched_location() {
        let (mut paula, _) = paula_with_collect_sink();
        paula.set_audio_min_period_cck(1);
        let dmacon = DMACON_DMAEN | 0x0001;

        // A stale playback pointer must be ignored: the DMA-enable edge
        // reloads it from AUDxLC.
        paula.chans[0].ptr = 0x20;
        paula.write_audio_reg(0x00, 0);
        paula.write_audio_reg(0x02, 4);
        paula.write_audio_reg(0x04, 2);
        paula.write_audio_reg(0x06, 8);

        assert_eq!(paula.advance_audio(0, dmacon) & INT_AUD0, INT_AUD0);
        // Pointer reloaded from AUDxLC; the first request reads word 0.
        let request = paula.audio_dma_request(0).unwrap();
        assert_eq!(request.address, 4);
        assert_eq!(paula.chans[0].ptr, 4);

        // The single first grant fetches word 0 and starts playback in
        // one slot (no separate pointer-reload grant). The pointer and
        // length counter step to the next word.
        assert_eq!(paula.grant_audio_dma(0, 0x5566) & INT_AUD0, 0);
        assert_eq!(paula.chans[0].state, ChanState::Running);
        assert_eq!(paula.chans[0].current, 0x55);
        assert_eq!(paula.chans[0].ptr, 6);
        assert_eq!(paula.chans[0].words_left, 1);
    }

    #[test]
    fn audio_dma_buffer_end_defers_pointer_reload_until_next_fetch() {
        let (mut paula, _) = paula_with_collect_sink();
        paula.set_audio_min_period_cck(1);
        let dmacon = DMACON_DMAEN | 0x0001;

        // Two-word buffer at AUDxLC=0.
        paula.write_audio_reg(0x00, 0);
        paula.write_audio_reg(0x02, 0);
        paula.write_audio_reg(0x04, 2);
        paula.write_audio_reg(0x06, 4);

        // DMA-enable edge fetches word 0 and starts playback.
        assert_eq!(paula.advance_audio(0, dmacon) & INT_AUD0, INT_AUD0);
        assert_eq!(paula.grant_audio_dma(0, 0x1020) & INT_AUD0, 0);
        assert_eq!(paula.chans[0].ptr, 2);
        assert_eq!(paula.chans[0].words_left, 1);

        // Playing word 0 arms the fetch of word 1 (the buffer's last
        // word) into the holding register.
        paula.advance_audio(1, dmacon);
        let request = paula.audio_dma_request(0).unwrap();
        assert_eq!(request.address, 2);

        // Fetching the last word wraps the length counter and raises the
        // channel interrupt, but the AUDxLC/AUDxLEN reload is DEFERRED to
        // the next buffer fetch (real Paula's one-word gap between the
        // interrupt and the pointer reload). The pointer just steps past
        // the buffer for now; no reload has happened yet.
        assert_eq!(paula.grant_audio_dma(0, 0x3040) & INT_AUD0, INT_AUD0);
        assert_eq!(paula.chans[0].ptr, 4);
        assert_eq!(paula.chans[0].words_left, 0);

        // The interrupt handler repoints the channel *after* the wrapping
        // fetch -- exactly what a one-shot sample does to point AUDxLC at a
        // silence loop. Because the reload was deferred, this is the value
        // the next buffer latches.
        paula.write_audio_reg(0x02, 8);

        // Advance until the channel arms its next buffer fetch; the
        // deferred reload then points at the handler's AUDxLC=8 with the
        // length counter refilled from AUDxLEN.
        let mut guard = 0;
        let next = loop {
            paula.advance_audio(1, dmacon);
            if let Some(req) = paula.audio_dma_request(0) {
                break req;
            }
            guard += 1;
            assert!(guard <= 24, "channel should arm the next buffer fetch");
        };
        assert_eq!(next.address, 8);
        assert_eq!(paula.chans[0].words_left, 2);
    }

    #[test]
    fn audio_dma_enable_mid_manual_period_restarts_from_location_registers() {
        let (mut paula, _) = paula_with_collect_sink();
        let mut ram = vec![0u8; 64];
        ram[0] = 0x51;
        ram[1] = 0x62;
        let dmacon = DMACON_DMAEN | 0x0001;

        paula.write_audio_reg(0x06, 8);
        paula.write_audio_reg(0x0A, 0x1020);
        assert_eq!(paula.tick_audio(3, 0, &ram) & INT_AUD0, 0);
        assert_eq!(paula.chans[0].current, 0x10);
        assert_eq!(paula.chans[0].period_acc, 3);

        paula.write_audio_reg(0x00, 0);
        paula.write_audio_reg(0x02, 0);
        paula.write_audio_reg(0x04, 2);
        assert_eq!(paula.tick_audio(1, dmacon, &ram) & INT_AUD0, INT_AUD0);
        assert_eq!(paula.chans[0].current, 0x51);
        assert_eq!(paula.chans[0].state, ChanState::Running);
        assert_eq!(paula.chans[0].phase, 1);
        assert_eq!(paula.chans[0].period_acc, 1);
    }

    #[test]
    fn audio_dma_disable_mid_period_silences_until_reenabled() {
        let (mut paula, _) = paula_with_collect_sink();
        let mut ram = vec![0u8; 64];
        ram[0] = 0x12;
        ram[1] = 0x34;
        let dmacon = DMACON_DMAEN | 0x0001;

        paula.write_audio_reg(0x00, 0);
        paula.write_audio_reg(0x02, 0);
        paula.write_audio_reg(0x04, 2);
        paula.write_audio_reg(0x06, 8);
        assert_eq!(paula.tick_audio(1, dmacon, &ram) & INT_AUD0, INT_AUD0);
        assert_eq!(paula.tick_audio(3, dmacon, &ram) & INT_AUD0, 0);
        assert_eq!(paula.chans[0].current, 0x12);
        assert_eq!(paula.chans[0].period_acc, 4);

        assert_eq!(paula.tick_audio(1, 0, &ram) & INT_AUD0, 0);
        assert_eq!(paula.chans[0].state, ChanState::Off);
        assert_eq!(paula.chans[0].current, 0);

        assert_eq!(paula.tick_audio(1, dmacon, &ram) & INT_AUD0, INT_AUD0);
        assert_eq!(paula.chans[0].current, 0x12);
        assert_eq!(paula.chans[0].period_acc, 1);
    }

    #[test]
    fn audio_subminimum_period_reuses_dma_buffer_until_fetch_budget_recovers() {
        let (mut paula, _) = paula_with_collect_sink();
        let mut ram = vec![0u8; 64];
        ram[0] = 0x10;
        ram[1] = 0x20;
        ram[2] = 0x30;
        ram[3] = 0x40;
        let dmacon = DMACON_DMAEN | 0x0001;

        paula.write_audio_reg(0x00, 0);
        paula.write_audio_reg(0x02, 0);
        paula.write_audio_reg(0x04, 2);
        paula.write_audio_reg(0x06, 2);

        assert_eq!(paula.tick_audio(1, dmacon, &ram) & INT_AUD0, INT_AUD0);
        assert_eq!(paula.chans[0].current, 0x10);
        assert_eq!(paula.chans[0].word_hi, 0x10);
        assert_eq!(paula.chans[0].ptr, 2);
        assert_eq!(paula.chans[0].words_left, 1);

        for _ in 0..60 {
            assert_eq!(paula.tick_audio(2, dmacon, &ram) & INT_AUD0, 0);
            assert_eq!(paula.chans[0].word_hi, 0x10);
            assert_eq!(paula.chans[0].ptr, 2);
            assert_eq!(paula.chans[0].words_left, 1);
        }

        // Once the per-line fetch budget recovers, the next word is
        // fetched. That fetch wraps the two-word length counter and
        // raises the channel interrupt, so do not require it to stay
        // clear here.
        let mut cck_until_second_word = 0;
        while paula.chans[0].word_hi != 0x30 {
            cck_until_second_word += 1;
            paula.tick_audio(1, dmacon, &ram);
            assert!(cck_until_second_word <= 16);
        }
        assert!(cck_until_second_word >= 4);
        assert_eq!(paula.chans[0].word_lo, 0x40);
    }

    #[test]
    fn audio_period_zero_holds_current_sample_without_irq_underflow() {
        let (mut paula, _) = paula_with_collect_sink();
        let mut ram = vec![0u8; 64];
        ram[0] = 0x55;
        ram[1] = 0x66;
        let dmacon = DMACON_DMAEN | 0x0001;

        paula.write_audio_reg(0x00, 0);
        paula.write_audio_reg(0x02, 0);
        paula.write_audio_reg(0x04, 2);
        paula.write_audio_reg(0x06, 0);

        assert_eq!(paula.tick_audio(1, dmacon, &ram) & INT_AUD0, INT_AUD0);
        assert_eq!(paula.chans[0].current, 0x55);
        assert_eq!(paula.tick_audio(64, dmacon, &ram) & INT_AUD0, 0);
        assert_eq!(paula.chans[0].current, 0x55);
        assert_eq!(paula.chans[0].phase, 1);
    }

    #[test]
    fn audio_period_write_uses_existing_countdown_phase() {
        let (mut paula, _) = paula_with_collect_sink();
        paula.set_audio_min_period_cck(1);
        let mut ram = vec![0u8; 64];
        ram[0] = 0x12;
        ram[1] = 0x34;
        let dmacon = DMACON_DMAEN | 0x0001;

        paula.write_audio_reg(0x00, 0);
        paula.write_audio_reg(0x02, 0);
        paula.write_audio_reg(0x04, 2);
        paula.write_audio_reg(0x06, 10);

        assert_eq!(paula.tick_audio(1, dmacon, &ram) & INT_AUD0, INT_AUD0);
        assert_eq!(paula.tick_audio(4, dmacon, &ram) & INT_AUD0, 0);
        assert_eq!(paula.chans[0].period_acc, 5);

        paula.write_audio_reg(0x06, 7);
        assert_eq!(paula.tick_audio(1, dmacon, &ram) & INT_AUD0, 0);
        assert_eq!(paula.chans[0].current, 0x12);
        assert_eq!(paula.tick_audio(1, dmacon, &ram) & INT_AUD0, 0);
        assert_eq!(paula.chans[0].current, 0x34);
    }

    #[test]
    fn audio_irq_asserts_on_final_low_byte_boundary() {
        let (mut paula, _) = paula_with_collect_sink();
        paula.set_audio_min_period_cck(1);
        let mut ram = vec![0u8; 64];
        ram[0] = 0x10;
        ram[1] = 0x20;
        ram[2] = 0x30;
        ram[3] = 0x40;
        let dmacon = DMACON_DMAEN | 0x0001;

        paula.write_audio_reg(0x00, 0);
        paula.write_audio_reg(0x02, 0);
        paula.write_audio_reg(0x04, 2);
        paula.write_audio_reg(0x06, 4);

        assert_eq!(paula.tick_audio(1, dmacon, &ram) & INT_AUD0, INT_AUD0);
        assert_eq!(paula.chans[0].current, 0x10);
        assert_eq!(paula.tick_audio(3, dmacon, &ram) & INT_AUD0, 0);
        assert_eq!(paula.chans[0].current, 0x20);
        assert_eq!(paula.chans[0].word_hi, 0x30);

        assert_eq!(paula.tick_audio(4, dmacon, &ram) & INT_AUD0, 0);
        assert_eq!(paula.chans[0].current, 0x30);
        assert_eq!(paula.tick_audio(3, dmacon, &ram) & INT_AUD0, 0);
        assert_eq!(paula.chans[0].current, 0x30);
        assert_eq!(paula.tick_audio(1, dmacon, &ram) & INT_AUD0, INT_AUD0);
        assert_eq!(paula.chans[0].current, 0x40);
    }

    #[test]
    fn cpu_auddat_write_during_dma_does_not_replace_dma_sample_word() {
        let (mut paula, _) = paula_with_collect_sink();
        paula.set_audio_min_period_cck(1);
        let mut ram = vec![0u8; 64];
        ram[0] = 0x11;
        ram[1] = 0x22;
        ram[2] = 0x33;
        ram[3] = 0x44;
        let dmacon = DMACON_DMAEN | 0x0001;

        paula.write_audio_reg(0x00, 0);
        paula.write_audio_reg(0x02, 0);
        paula.write_audio_reg(0x04, 2);
        paula.write_audio_reg(0x06, 4);

        assert_eq!(paula.tick_audio(1, dmacon, &ram) & INT_AUD0, INT_AUD0);
        assert_eq!(paula.chans[0].current, 0x11);
        paula.write_audio_reg(0x0A, 0x7F7E);
        assert_eq!(paula.peek_audio_reg_latch(0x0A), Some(0x7F7E));

        assert_eq!(paula.tick_audio(3, dmacon, &ram) & INT_AUD0, 0);
        assert_eq!(paula.chans[0].current, 0x22);
        assert_eq!(paula.chans[0].word_hi, 0x33);
        assert_eq!(paula.chans[0].word_lo, 0x44);
        // The two-word buffer has wrapped and the pointer reloaded, with
        // one word already fetched ahead into the holding register.
        assert_eq!(paula.chans[0].ptr, 2);
        assert_eq!(paula.chans[0].words_left, 1);
    }

    #[test]
    fn cpu_auddat_latch_during_dma_does_not_survive_as_manual_restart() {
        let (mut paula, _) = paula_with_collect_sink();
        let mut ram = vec![0u8; 64];
        ram[0] = 0x12;
        ram[1] = 0x34;
        let dmacon = DMACON_DMAEN | 0x0001;

        paula.write_audio_reg(0x00, 0);
        paula.write_audio_reg(0x02, 0);
        paula.write_audio_reg(0x04, 1);
        paula.write_audio_reg(0x06, 8);

        assert_eq!(paula.tick_audio(1, dmacon, &ram) & INT_AUD0, INT_AUD0);
        paula.write_audio_reg(0x0A, 0x5566);
        assert_eq!(paula.tick_audio(1, dmacon, &ram) & INT_AUD0, 0);
        assert!(!paula.chans[0].manual_pending);

        assert_eq!(paula.tick_audio(1, 0, &ram) & INT_AUD0, 0);
        assert_eq!(paula.chans[0].state, ChanState::Off);
        assert_eq!(paula.chans[0].current, 0);
    }

    #[test]
    fn dma_buffer_reload_ignores_cpu_auddat_latch() {
        let (mut paula, _) = paula_with_collect_sink();
        paula.set_audio_min_period_cck(1);
        let mut ram = vec![0u8; 64];
        ram[0] = 0x10;
        ram[1] = 0x20;
        ram[4] = 0x30;
        ram[5] = 0x40;
        let dmacon = DMACON_DMAEN | 0x0001;

        paula.write_audio_reg(0x00, 0);
        paula.write_audio_reg(0x02, 0);
        paula.write_audio_reg(0x04, 1);
        paula.write_audio_reg(0x06, 4);
        assert_eq!(paula.tick_audio(1, dmacon, &ram) & INT_AUD0, INT_AUD0);

        paula.write_audio_reg(0x02, 4);
        paula.write_audio_reg(0x0A, 0x7F7F);
        assert_eq!(paula.tick_audio(3, dmacon, &ram) & INT_AUD0, INT_AUD0);
        assert_eq!(paula.chans[0].current, 0x20);
        // The active word comes from audio DMA (0x1020 here, the word in
        // flight through the fetch-ahead pipeline), never the ignored CPU
        // AUDxDAT latch (0x7F7F).
        assert_eq!(paula.chans[0].word_hi, 0x10);
        assert_eq!(paula.chans[0].word_lo, 0x20);
        assert_eq!(paula.peek_audio_reg_latch(0x0A), Some(0x7F7F));
    }

    #[test]
    fn wav_capture_records_deterministic_paula_dma_window() {
        let path =
            std::env::temp_dir().join(format!("copperline-paula-dma-window-{}.wav", process::id()));
        let _ = std::fs::remove_file(&path);

        {
            let wav = WavSink::new(&path).expect("create wav sink");
            let mut paula = Paula::new(Box::new(NoopSerial), Box::new(wav));
            paula.set_led_filter_enabled(false);
            let mut ram = vec![0u8; 64];
            ram[0] = 0x40;
            ram[1] = 0xC0;
            ram[2] = 0x40;
            ram[3] = 0xC0;

            paula.write_audio_reg(0x00, 0);
            paula.write_audio_reg(0x02, 0);
            paula.write_audio_reg(0x04, 2);
            paula.write_audio_reg(0x06, 400);
            paula.write_audio_reg(0x08, 64);
            let _ = paula.tick_audio(1_000, DMACON_DMAEN | 0x0001, &ram);
        }

        let mut reader = hound::WavReader::open(&path).expect("open wav");
        let spec = reader.spec();
        assert_eq!(spec.channels, 2);
        assert_eq!(spec.sample_rate, MIX_SAMPLE_RATE);
        assert_eq!(spec.bits_per_sample, 32);
        assert_eq!(spec.sample_format, hound::SampleFormat::Float);

        let samples = reader
            .samples::<f32>()
            .take(24)
            .collect::<Result<Vec<_>, _>>()
            .expect("read wav samples");
        assert_eq!(samples.len(), 24);
        let frames = samples
            .chunks_exact(2)
            .map(|frame| (frame[0], frame[1]))
            .collect::<Vec<_>>();
        let expected_left = [
            0.5, 0.5, 0.5, 0.5, -0.5, -0.5, -0.5, -0.5, -0.5, 0.5, 0.5, 0.5,
        ];
        for ((left, right), expected_left) in frames.iter().copied().zip(expected_left) {
            assert!((left - expected_left).abs() < f32::EPSILON);
            assert_eq!(right, 0.0);
        }

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn wav_capture_routes_paula_channels_to_stereo_pairs() {
        let path = std::env::temp_dir().join(format!(
            "copperline-paula-channel-routing-{}.wav",
            process::id()
        ));
        let _ = std::fs::remove_file(&path);

        {
            let wav = WavSink::new(&path).expect("create wav sink");
            let mut paula = Paula::new(Box::new(NoopSerial), Box::new(wav));
            paula.set_led_filter_enabled(false);

            for ch_idx in 0..4 {
                paula.chans[ch_idx].current = 64;
                paula.chans[ch_idx].vol = 64;
                paula.push_mixed_frame();
                paula.chans[ch_idx].current = 0;
            }
        }

        let mut reader = hound::WavReader::open(&path).expect("open wav");
        let spec = reader.spec();
        assert_eq!(spec.channels, 2);
        assert_eq!(spec.sample_rate, MIX_SAMPLE_RATE);
        assert_eq!(spec.bits_per_sample, 32);
        assert_eq!(spec.sample_format, hound::SampleFormat::Float);

        let samples = reader
            .samples::<f32>()
            .collect::<Result<Vec<_>, _>>()
            .expect("read wav samples");
        let frames = samples
            .chunks_exact(2)
            .map(|frame| (frame[0], frame[1]))
            .collect::<Vec<_>>();

        assert_eq!(frames, &[(0.5, 0.0), (0.0, 0.5), (0.0, 0.5), (0.5, 0.0)]);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn serdat_uses_timed_transmit_shift_register() {
        let (mut paula, written, _) = paula_with_collect_serial();

        assert_eq!(paula.write_serdat(0x0141), INT_TBE);
        assert_eq!(paula.next_serial_event_cck(), Some(1));
        assert!(!paula.serial_txd_pin_high());
        assert_ne!(paula.read_serdatr() & (1 << 13), 0);
        assert_eq!(paula.read_serdatr() & (1 << 12), 0);
        assert!(written.lock().unwrap().is_empty());

        assert_eq!(paula.tick_serial(1), 0);
        assert!(paula.serial_txd_pin_high());
        assert_eq!(paula.tick_serial(8), 0);
        assert_eq!(paula.next_serial_event_cck(), Some(1));
        assert!(paula.serial_txd_pin_high());
        assert!(written.lock().unwrap().is_empty());
        assert_eq!(paula.tick_serial(1), 0);
        assert_eq!(paula.next_serial_event_cck(), None);
        assert_eq!(&*written.lock().unwrap(), &[0x41]);
        assert_ne!(paula.read_serdatr() & (1 << 12), 0);
    }

    #[test]
    fn serdat_masks_stop_bit_and_preserves_long_data_bit_for_word_sinks() {
        let (mut paula, written, _) = paula_with_collect_serial_words();

        assert_eq!(paula.write_serdat(0x0141), INT_TBE);
        assert_eq!(paula.tick_serial(10), 0);
        assert_eq!(&*written.lock().unwrap(), &[(0x0041, false)]);

        paula.serper = SERPER_LONG;
        assert_eq!(paula.write_serdat(0x0342), INT_TBE);
        assert_eq!(paula.tick_serial(10), 0);
        assert_eq!(&*written.lock().unwrap(), &[(0x0041, false)]);
        assert_eq!(paula.tick_serial(1), 0);
        assert_eq!(
            &*written.lock().unwrap(),
            &[(0x0041, false), (0x0142, true)]
        );
    }

    #[test]
    fn serper_long_extends_serial_word_timing() {
        let (mut paula, written, _) = paula_with_collect_serial();
        paula.serper = SERPER_LONG;

        assert_eq!(paula.write_serdat(0x0341), INT_TBE);
        assert_eq!(paula.tick_serial(10), 0);
        assert!(written.lock().unwrap().is_empty());
        assert_eq!(paula.tick_serial(1), 0);
        assert_eq!(&*written.lock().unwrap(), &[0x41]);
    }

    #[test]
    fn adkcon_uartbrk_forces_serial_txd_low() {
        let (mut paula, written, _) = paula_with_collect_serial();

        assert_eq!(paula.write_serdat(0x01FF), INT_TBE);
        assert!(!paula.serial_txd_pin_high());
        assert_eq!(paula.tick_serial(1), 0);
        assert!(paula.serial_txd_pin_high());

        paula.write_adkcon(0x8000 | ADKCON_UARTBRK);
        assert!(!paula.serial_txd_pin_high());
        assert_eq!(paula.tick_serial(9), 0);
        assert!(written.lock().unwrap().is_empty());

        paula.write_adkcon(ADKCON_UARTBRK);
        assert!(paula.serial_txd_pin_high());
    }

    #[test]
    fn serdatr_reports_receive_buffer_and_overrun_until_rbf_clear() {
        let (mut paula, _, read) = paula_with_collect_serial();
        read.lock().unwrap().extend_from_slice(&[0x55, 0x66]);

        assert_eq!(paula.tick_serial(9) & INT_RBF, 0);
        let irq = paula.tick_serial(1);
        assert_eq!(irq & INT_RBF, INT_RBF);
        paula.latch_interrupt_sources(irq);
        let serdatr = paula.read_serdatr();
        assert_ne!(serdatr & (1 << 14), 0);
        assert_eq!(serdatr & (1 << 15), 0);
        assert_eq!(serdatr & 0x00FF, 0x55);
        assert_eq!(serdatr & 0x0300, 0x0300);

        assert_eq!(paula.tick_serial(10) & INT_RBF, 0);
        assert_ne!(paula.read_serdatr() & (1 << 15), 0);

        paula.write_intreq(INT_RBF);
        assert_eq!(paula.read_serdatr() & ((1 << 15) | (1 << 14)), 0);
    }

    #[test]
    fn serper_long_receive_keeps_ninth_data_bit() {
        let (mut paula, _, read) = paula_with_collect_serial_words();
        paula.serper = SERPER_LONG;
        read.lock().unwrap().push(0x0155);

        assert_eq!(paula.tick_serial(10) & INT_RBF, 0);
        let irq = paula.tick_serial(1);
        assert_eq!(irq & INT_RBF, INT_RBF);
        paula.latch_interrupt_sources(irq);
        let serdatr = paula.read_serdatr();
        assert_ne!(serdatr & (1 << 14), 0);
        assert_eq!(serdatr & 0x03FF, 0x0355);
    }

    #[test]
    fn serdatr_rxd_uses_two_stage_synchronized_pin() {
        let (mut paula, _, read) = paula_with_collect_serial();
        paula.serper = 3;
        read.lock().unwrap().push(0x01);

        assert_eq!(paula.tick_serial(0), 0);
        assert_ne!(paula.read_serdatr() & (1 << 11), 0);
        assert_eq!(paula.tick_serial(1), 0);
        assert_ne!(paula.read_serdatr() & (1 << 11), 0);
        assert_eq!(paula.tick_serial(1), 0);
        assert_eq!(paula.read_serdatr() & (1 << 11), 0);

        assert_eq!(paula.tick_serial(2), 0);
        assert_eq!(paula.read_serdatr() & (1 << 11), 0);
        assert_eq!(paula.tick_serial(1), 0);
        assert_eq!(paula.read_serdatr() & (1 << 11), 0);
        assert_eq!(paula.tick_serial(1), 0);
        assert_ne!(paula.read_serdatr() & (1 << 11), 0);
    }

    #[test]
    fn adkcon_audio_modulation_alternates_volume_then_period_when_both_attached() {
        let (mut paula, _) = paula_with_collect_sink();
        let mut ram = vec![0u8; 64];
        ram[0] = 0x12;
        ram[1] = 0x34;

        paula.chans[0].state = ChanState::Running;
        paula.chans[0].per = 1;
        paula.chans[0].word_hi = 0;
        paula.chans[0].word_lo = 63;
        paula.chans[0].phase = 0;
        paula.chans[0].ptr = 0;
        paula.chans[0].words_left = 1;
        paula.chans[1].vol = 1;
        paula.chans[1].per = 100;
        paula.last_dmacon = DMACON_DMAEN | 0x0001;
        paula.write_adkcon(0x8000 | 0x0011);

        paula.tick_audio(1, DMACON_DMAEN | 0x0001, &ram);

        assert_eq!(paula.chans[1].vol, 63);
        assert_eq!(paula.chans[1].per, 100);

        paula.tick_audio(2, DMACON_DMAEN | 0x0001, &ram);

        assert_eq!(paula.chans[1].vol, 63);
        assert_eq!(paula.chans[1].per, 0x1234);
    }

    #[test]
    fn adkcon_volume_modulation_can_enable_and_disable_mid_stream() {
        let (mut paula, _) = paula_with_collect_sink();
        let ram = vec![0u8; 64];
        let dmacon = DMACON_DMAEN | 0x0001;

        paula.chans[0].state = ChanState::Running;
        paula.chans[0].per = 4;
        paula.chans[0].word_hi = 0x12;
        paula.chans[0].word_lo = 0x34;
        paula.chans[0].phase = 0;
        // Several words still to fetch, so the fetch-ahead prefetch does
        // not wrap the length counter (which would raise AUDx) during
        // this short modulation window.
        paula.chans[0].words_left = 8;
        paula.chans[1].vol = 1;
        paula.last_dmacon = dmacon;

        assert_eq!(paula.tick_audio(3, dmacon, &ram) & INT_AUD0, 0);
        assert_eq!(paula.chans[1].vol, 1);

        paula.write_adkcon(0x8000 | 0x0001);
        assert_eq!(paula.tick_audio(1, dmacon, &ram) & INT_AUD0, 0);
        assert_eq!(paula.chans[1].vol, 0x34);

        paula.write_adkcon(0x0001);
        assert_eq!(paula.tick_audio(4, dmacon, &ram) & INT_AUD0, 0);
        assert_eq!(paula.chans[1].vol, 0x34);
    }

    #[test]
    fn adkcon_volume_modulation_uses_low_seven_bits_of_word() {
        let (mut paula, _) = paula_with_collect_sink();
        let ram = vec![0u8; 64];
        let dmacon = DMACON_DMAEN | 0x0001;

        paula.chans[0].state = ChanState::Running;
        paula.chans[0].per = 4;
        paula.chans[0].word_hi = 0xFFu8 as i8;
        paula.chans[0].word_lo = 0x3F;
        paula.chans[0].phase = 0;
        // Several words still to fetch, so the fetch-ahead prefetch does
        // not wrap the length counter during this short window.
        paula.chans[0].words_left = 8;
        paula.chans[1].vol = 1;
        paula.last_dmacon = dmacon;
        paula.write_adkcon(0x8000 | 0x0001);

        assert_eq!(paula.tick_audio(4, dmacon, &ram) & INT_AUD0, 0);
        assert_eq!(paula.chans[1].vol, 0x3F);

        paula.chans[0].word_hi = 0x80u8 as i8;
        paula.chans[0].word_lo = 0x7F;
        paula.chans[0].phase = 0;
        assert_eq!(paula.tick_audio(4, dmacon, &ram) & INT_AUD0, 0);
        assert_eq!(paula.chans[1].vol, 64);
    }

    #[test]
    fn adkcon_period_modulation_clamps_zero_and_keeps_tiny_periods() {
        let (mut paula, _) = paula_with_collect_sink();
        let ram = vec![0u8; 64];
        let dmacon = DMACON_DMAEN | 0x0004;

        paula.chans[2].state = ChanState::Running;
        paula.chans[2].per = 1;
        paula.chans[2].word_hi = 0x00;
        paula.chans[2].word_lo = 0x00;
        paula.chans[2].phase = 0;
        // Several words still to fetch, so the fetch-ahead prefetch does
        // not wrap the length counter during this short window.
        paula.chans[2].words_left = 8;
        paula.chans[3].per = 100;
        paula.last_dmacon = dmacon;
        paula.write_adkcon(0x8000 | 0x0040);

        assert_eq!(paula.tick_audio(1, dmacon, &ram) & INT_AUD2, 0);
        assert_eq!(paula.chans[3].per, 1);

        paula.chans[2].word_hi = 0x00;
        paula.chans[2].word_lo = 0x02;
        paula.chans[2].phase = 0;
        assert_eq!(paula.tick_audio(1, dmacon, &ram) & INT_AUD2, 0);
        assert_eq!(paula.chans[3].per, 2);
    }

    #[test]
    fn adkcon_modulation_preserves_source_and_target_irq_cadence() {
        let (mut paula, _) = paula_with_collect_sink();
        paula.set_audio_min_period_cck(1);
        let mut ram = vec![0u8; 64];
        ram[0] = 0x20;
        ram[1] = 0x10;
        ram[2] = 0x44;
        ram[3] = 0x55;
        ram[8] = 0x30;
        ram[9] = 0x40;
        let dmacon = DMACON_DMAEN | 0x0003;

        paula.write_audio_reg(0x00, 0);
        paula.write_audio_reg(0x02, 0);
        paula.write_audio_reg(0x04, 1);
        paula.write_audio_reg(0x06, 4);
        paula.write_audio_reg(0x10, 0);
        paula.write_audio_reg(0x12, 8);
        paula.write_audio_reg(0x14, 1);
        paula.write_audio_reg(0x16, 6);
        paula.write_audio_reg(0x18, 1);
        paula.write_adkcon(0x8000 | 0x0001);

        assert_eq!(
            paula.tick_audio(1, dmacon, &ram) & (INT_AUD0 | INT_AUD1),
            INT_AUD0 | INT_AUD1
        );
        assert_eq!(paula.chans[1].vol, 0x10);

        assert_eq!(
            paula.tick_audio(3, dmacon, &ram) & (INT_AUD0 | INT_AUD1),
            INT_AUD0
        );
        assert_eq!(paula.chans[1].vol, 0x10);

        assert_eq!(
            paula.tick_audio(2, dmacon, &ram) & (INT_AUD0 | INT_AUD1),
            INT_AUD1
        );
    }

    #[test]
    fn adkcon_attached_source_channel_is_not_mixed_to_dac() {
        let (mut paula, frames) = paula_with_collect_sink();
        paula.set_led_filter_enabled(false);
        paula.chans[0].state = ChanState::ManualHold;
        paula.chans[0].current = 127;
        paula.chans[0].vol = 64;
        paula.chans[1].state = ChanState::ManualHold;
        paula.chans[1].current = 127;
        paula.chans[1].vol = 64;

        paula.write_adkcon(0x8000 | 0x0001);
        paula.advance_audio(PAULA_CLOCK_HZ.div_ceil(MIX_SAMPLE_RATE), 0);

        let frames = frames.borrow();
        let (left, right) = frames[0];
        assert_eq!(left, 0.0);
        assert!(right > 0.9, "target channel should remain audible: {right}");
    }

    #[test]
    fn led_filter_attenuates_high_frequency_output() {
        fn alternating_average(filter_enabled: bool) -> f32 {
            let (mut paula, frames) = paula_with_collect_sink();
            paula.set_led_filter_enabled(filter_enabled);
            paula.chans[0].vol = 64;
            for i in 0..256 {
                paula.chans[0].current = if i & 1 == 0 { 127 } else { -127 };
                paula.push_mixed_frame();
            }
            let frames = frames.borrow();
            let settled = &frames[64..];
            settled.iter().map(|(left, _)| left.abs()).sum::<f32>() / settled.len() as f32
        }

        let bypassed = alternating_average(false);
        let filtered = alternating_average(true);
        assert!(
            filtered < bypassed * 0.25,
            "LED filter should attenuate high-frequency alternation, bypassed={bypassed}, filtered={filtered}"
        );
    }

    #[test]
    fn led_filter_has_three_pole_four_kilohertz_shape() {
        fn gain_at(freq_hz: f32) -> f32 {
            let mut filter = AnalogLedFilter::new(LED_FILTER_CUTOFF_HZ, MIX_SAMPLE_RATE as f32);
            let sample_rate = MIX_SAMPLE_RATE as f32;
            let settle = 4096;
            let samples = MIX_SAMPLE_RATE as usize;
            let mut in_sq = 0.0;
            let mut out_sq = 0.0;
            for n in 0..samples {
                let phase = 2.0 * PI * freq_hz * n as f32 / sample_rate;
                let input = phase.sin();
                let output = filter.process(input);
                if n >= settle {
                    in_sq += input * input;
                    out_sq += output * output;
                }
            }
            (out_sq / in_sq).sqrt()
        }

        let low = gain_at(1_000.0);
        let knee = gain_at(LED_FILTER_CUTOFF_HZ);
        let high = gain_at(12_000.0);

        assert!(low > 0.75, "1 kHz should stay in passband: {low}");
        assert!(
            (0.35..0.70).contains(&knee),
            "4 kHz should be near the combined one-pole/two-pole knee: {knee}"
        );
        assert!(
            high < 0.15,
            "12 kHz should be strongly attenuated by the three-pole cascade: {high}"
        );
        assert!(low > knee * 1.5 && knee > high * 3.0);
    }

    #[test]
    fn wav_capture_led_filter_records_bypassed_and_filtered_levels() {
        fn alternating_wav_average(filter_enabled: bool, label: &str) -> f32 {
            let path = std::env::temp_dir().join(format!(
                "copperline-paula-led-filter-{label}-{}.wav",
                process::id()
            ));
            let _ = std::fs::remove_file(&path);

            {
                let wav = WavSink::new(&path).expect("create wav sink");
                let mut paula = Paula::new(Box::new(NoopSerial), Box::new(wav));
                paula.set_led_filter_enabled(filter_enabled);
                paula.chans[0].vol = 64;
                for i in 0..256 {
                    paula.chans[0].current = if i & 1 == 0 { 127 } else { -127 };
                    paula.push_mixed_frame();
                }
            }

            let mut reader = hound::WavReader::open(&path).expect("open wav");
            let spec = reader.spec();
            assert_eq!(spec.channels, 2);
            assert_eq!(spec.sample_rate, MIX_SAMPLE_RATE);
            let samples = reader
                .samples::<f32>()
                .collect::<Result<Vec<_>, _>>()
                .expect("read wav samples");
            let frames = samples.chunks_exact(2).collect::<Vec<_>>();
            let average = frames[64..].iter().map(|frame| frame[0].abs()).sum::<f32>()
                / (frames.len() - 64) as f32;

            let _ = std::fs::remove_file(&path);
            average
        }

        let bypassed = alternating_wav_average(false, "bypassed");
        let filtered = alternating_wav_average(true, "filtered");
        assert!(
            filtered < bypassed * 0.20,
            "WAV-level LED filter should attenuate alternating output, bypassed={bypassed}, filtered={filtered}"
        );
    }

    #[test]
    fn host_output_volume_scales_mixed_audio_without_changing_audvol() {
        let (mut paula, frames) = paula_with_collect_sink();
        paula.set_led_filter_enabled(false);
        paula.set_output_volume_percent(50);
        paula.chans[0].current = 64;
        paula.chans[0].vol = 64;

        paula.push_mixed_frame();

        let frames = frames.borrow();
        assert_eq!(paula.output_volume_percent(), 50);
        assert_eq!(paula.chans[0].vol, 64);
        assert!((frames[0].0 - 0.25).abs() < f32::EPSILON);
        assert_eq!(frames[0].1, 0.0);
    }

    #[test]
    fn pot_event_deadline_tracks_counter_increment() {
        let (mut paula, _) = paula_with_collect_sink();

        assert_eq!(paula.next_pot_event_cck(), None);
        paula.write_potgo(0x0001);
        assert_eq!(paula.next_pot_event_cck(), Some(POT_COUNTER_CCK));
        paula.tick_pots(POT_COUNTER_CCK - 1);
        assert_eq!(paula.next_pot_event_cck(), Some(1));
        paula.tick_pots(1);
        assert_eq!(paula.read_potdat(0), 0x0101);
        assert_eq!(paula.next_pot_event_cck(), Some(POT_COUNTER_CCK));

        paula.pot_counters = [u8::MAX; 4];
        paula.pot_acc_cck = POT_COUNTER_CCK - 1;
        paula.tick_pots(1);
        assert_eq!(paula.next_pot_event_cck(), None);
    }

    #[test]
    fn intreq_write_reports_only_new_assertions() {
        let (mut paula, _) = paula_with_collect_sink();

        assert!(paula.write_intreq(0x8004));
        assert_eq!(paula.intreq, 0x0004);
        assert!(!paula.write_intreq(0x8004));
        assert_eq!(paula.intreq, 0x0004);

        assert!(!paula.write_intreq(0x0004));
        assert_eq!(paula.intreq, 0x0000);
        assert!(paula.write_intreq(0x8004));
        assert_eq!(paula.intreq, 0x0004);
    }

    #[test]
    fn intreq_latches_undocumented_int14_source() {
        let (mut paula, _) = paula_with_collect_sink();

        assert!(paula.write_intreq(0x8000 | INT_INT14));
        assert_eq!(paula.intreq & INT_INT14, INT_INT14);

        assert!(!paula.write_intreq(INT_INT14));
        assert_eq!(paula.intreq & INT_INT14, 0);
    }

    #[test]
    fn intreq_source_latch_wins_over_same_tick_clear() {
        let (mut paula, _) = paula_with_collect_sink();

        paula.serial_rx_buffer = Some(0x0355);
        assert!(paula.write_intreq_with_source_bits(INT_RBF, INT_RBF));
        assert_eq!(paula.intreq & INT_RBF, INT_RBF);
        assert_eq!(paula.serial_rx_buffer, Some(0x0355));
    }

    #[test]
    fn serdatr_rbf_bit_mirrors_intreq_latch() {
        let (mut paula, _) = paula_with_collect_sink();

        assert_eq!(paula.read_serdatr() & (1 << 14), 0);
        assert!(paula.write_intreq(0x8000 | INT_RBF));
        assert_ne!(paula.read_serdatr() & (1 << 14), 0);

        assert!(!paula.write_intreq(INT_RBF));
        assert_eq!(paula.read_serdatr() & (1 << 14), 0);
    }

    #[test]
    fn audpen_bits_mirror_audio_intreq_latches() {
        let (mut paula, _) = paula_with_collect_sink();

        assert!(paula.write_intreq(0x8000 | INT_AUD0 | INT_AUD3));
        assert_eq!(paula.audpen_bits(), 0b1001);

        assert!(!paula.write_intreq(INT_AUD0));
        assert_eq!(paula.audpen_bits(), 0b1000);
    }

    #[test]
    fn pending_ipl_maps_int14_to_level_six_priority() {
        assert_eq!(pending_ipl(INT_INT14), 6);
        assert_eq!(pending_ipl(INT_INT14 | INT_RBF), 6);
        assert_eq!(pending_ipl(INT_RBF), 5);
        assert_eq!(pending_ipl(INT_AUD0), 4);
        assert_eq!(pending_ipl(INT_TBE), 1);
    }
}
