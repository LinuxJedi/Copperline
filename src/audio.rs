// SPDX-License-Identifier: GPL-3.0-or-later

//! Audio output sink. Paula's mixed stereo output is funneled through
//! here. Mirrors the shape of [`crate::serial::SerialSink`]: a trait
//! plus several concrete implementations chosen at startup time based
//! on CLI flags.

use std::fs::File;
use std::io::BufWriter;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Instant;

use anyhow::{anyhow, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use ringbuf::traits::{Consumer, Observer, Producer, Split};
use ringbuf::HeapRb;

/// Sample rate the mixer feeds the sink at. This is the rate the
/// emulator-side stereo mixer (Paula::tick_audio) runs at; live CPAL
/// output resamples these frames to the selected device rate.
pub const MIX_SAMPLE_RATE: u32 = 44_100;
const PAL_PAULA_CLOCK_HZ: u32 = 3_546_895;
const AUDIO_PROFILE_ENV: &str = "COPPERLINE_AUDIO_PROFILE";
const CPAL_BUFFER_FRAMES: usize = 131072;
// Live-output latency budget. The steady-state target is deliberately fixed:
// the emulator's real-time pacer runs the core ahead of the wall clock by
// `CPAL_TARGET_BUFFER_FRAMES` worth of audio (it subtracts
// `live_output_lead_seconds` from its device-time target). If a host hitch
// drains an already-started queue below target, the sink reports the shortfall
// as extra temporary lead so the pacer refills the fixed cushion instead of
// settling into a fragile low-latency state.
const CPAL_TARGET_BUFFER_FRAMES: usize = 6615; // ~150 ms steady lead
const CPAL_PREBUFFER_FRAMES: usize = CPAL_TARGET_BUFFER_FRAMES;
const CPAL_REBUFFER_THRESHOLD_FRAMES: usize = 512; // stop before the callback drains dry
const CPAL_STALE_DROP_THRESHOLD_FRAMES: usize = 13230; // trim only past ~300 ms

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct AudioRuntimeStatus {
    pub queue_depth_frames: usize,
    pub output_lead_seconds: f64,
    pub callback_underrun_frames: u64,
    pub dropped_overrun_frames: u64,
    pub skipped_stale_frames: u64,
}

// Note: this trait is intentionally *not* `Send`. CpalSink owns a
// `cpal::Stream` which is `!Send` on macOS (the CoreAudio backend
// uses non-thread-safe Objective-C types internally). The whole
// emulator runs on the winit main thread, so no thread crossing
// happens. The cpal callback thread receives samples via the
// internal ring buffer, whose producer half *is* Send.
pub trait AudioSink {
    /// Push one stereo frame of normalised samples. Both inputs are
    /// expected to be in roughly [-1.0, 1.0] though we don't clip.
    fn push(&mut self, left: f32, right: f32);
    fn flush(&mut self);
    /// Suspend only the host live-output stream while emulation is
    /// intentionally not producing samples. Offline sinks can ignore this.
    fn set_live_output_suspended(&mut self, _suspended: bool) {}
    fn live_output_lead_seconds(&self) -> f64 {
        0.0
    }
    fn runtime_status(&self) -> AudioRuntimeStatus {
        AudioRuntimeStatus::default()
    }
}

pub fn audio_profile_enabled() -> bool {
    crate::envcfg::flag(AUDIO_PROFILE_ENV)
}

// -----------------------------------------------------------------
// NullSink: drops everything. Used when --noaudio is passed without
// --audio-wav.
// -----------------------------------------------------------------

pub struct NullSink;

impl AudioSink for NullSink {
    fn push(&mut self, _left: f32, _right: f32) {}
    fn flush(&mut self) {}
}

// -----------------------------------------------------------------
// CpalSink: writes mixer frames into a single-producer/single-consumer
// ring buffer; a cpal output stream pulls from the consumer half on
// its own callback thread. Underruns are silently filled with zeros
// and counted. When the producer overruns, the callback is asked to
// drop old queued frames so live output stays near the emulator's
// current Paula state instead of playing a stale backlog.
// -----------------------------------------------------------------

pub struct CpalSink {
    producer: ringbuf::HeapProd<(f32, f32)>,
    // Keep the stream alive for the lifetime of the sink.
    _stream: cpal::Stream,
    playback_started: Arc<AtomicBool>,
    drop_old_frames: Arc<AtomicUsize>,
    dropped_old_frames: Arc<AtomicU64>,
    total_dropped_old_frames: Arc<AtomicU64>,
    underruns: Arc<AtomicU64>,
    total_underruns: Arc<AtomicU64>,
    live_output_suspended: Arc<AtomicBool>,
    profile_callbacks: Arc<AtomicU64>,
    profile_callback_frames: Arc<AtomicU64>,
    profile_callback_device_cck: Arc<AtomicU64>,
    profile_enabled: bool,
    generated_frames: u64,
    overruns: u64,
    total_overruns: u64,
    last_log: Instant,
    prebuffer_frames: usize,
}

impl CpalSink {
    /// Build the live cpal output sink. When `realtime_priority` is set, the
    /// audio callback thread promotes itself on its first invocation (see
    /// [`crate::priority`]); the flag is resolved by the caller from config and
    /// the `COPPERLINE_REALTIME_PRIORITY` env var.
    pub fn new(realtime_priority: bool) -> Result<Self> {
        let host = cpal::default_host();
        let device = host
            .default_output_device()
            .ok_or_else(|| anyhow!("no default audio output device"))?;
        let supported = device
            .default_output_config()
            .map_err(|e| anyhow!("query default output config: {e}"))?;

        // Paula mixing always feeds f32 stereo at MIX_SAMPLE_RATE.
        // The host stream uses the device's default rate and the
        // callback performs a small linear resample so live output
        // doesn't slowly drain or grow when the device is e.g. 48 kHz.
        let channels = supported.channels().max(2);
        let output_sample_rate = supported.sample_rate().0;
        let config = cpal::StreamConfig {
            channels,
            sample_rate: supported.sample_rate(),
            buffer_size: cpal::BufferSize::Default,
        };

        // ~3 s capacity at 44.1 kHz. Playback starts after a short
        // prebuffer so live output comes up promptly; stale-frame
        // dropping still trims back to a bounded latency target.
        let rb = HeapRb::<(f32, f32)>::new(CPAL_BUFFER_FRAMES);
        let (producer, mut consumer) = rb.split();

        let prebuffer_frames = CPAL_PREBUFFER_FRAMES;
        let playback_started = Arc::new(AtomicBool::new(false));
        let playback_started_for_cb = Arc::clone(&playback_started);
        let drop_old_frames = Arc::new(AtomicUsize::new(0));
        let drop_old_frames_for_cb = Arc::clone(&drop_old_frames);
        let dropped_old_frames = Arc::new(AtomicU64::new(0));
        let dropped_old_frames_for_cb = Arc::clone(&dropped_old_frames);
        let total_dropped_old_frames = Arc::new(AtomicU64::new(0));
        let total_dropped_old_frames_for_cb = Arc::clone(&total_dropped_old_frames);
        let underruns = Arc::new(AtomicU64::new(0));
        let underruns_for_cb = Arc::clone(&underruns);
        let total_underruns = Arc::new(AtomicU64::new(0));
        let total_underruns_for_cb = Arc::clone(&total_underruns);
        let live_output_suspended = Arc::new(AtomicBool::new(false));
        let live_output_suspended_for_cb = Arc::clone(&live_output_suspended);
        let profile_enabled = audio_profile_enabled();
        let profile_callbacks = Arc::new(AtomicU64::new(0));
        let profile_callbacks_for_cb = Arc::clone(&profile_callbacks);
        let profile_callback_frames = Arc::new(AtomicU64::new(0));
        let profile_callback_frames_for_cb = Arc::clone(&profile_callback_frames);
        let profile_callback_device_cck = Arc::new(AtomicU64::new(0));
        let profile_callback_device_cck_for_cb = Arc::clone(&profile_callback_device_cck);
        let mut resampler = CpalResampler::new(output_sample_rate);

        let stream = device
            .build_output_stream(
                &config,
                move |data: &mut [f32], _info: &cpal::OutputCallbackInfo| {
                    // Runs on the cpal-owned audio thread. Latched internally,
                    // so only the first callback does the scheduling syscall.
                    if realtime_priority {
                        crate::priority::promote_audio_thread_once();
                    }
                    let chans = channels as usize;
                    if profile_enabled {
                        let frames = data.len() / chans;
                        profile_callbacks_for_cb.fetch_add(1, Ordering::Relaxed);
                        profile_callback_frames_for_cb.fetch_add(frames as u64, Ordering::Relaxed);
                        profile_callback_device_cck_for_cb.fetch_add(
                            callback_device_cck(frames, output_sample_rate),
                            Ordering::Relaxed,
                        );
                    }
                    if live_output_suspended_for_cb.load(Ordering::Relaxed) {
                        resampler.reset();
                        for sample in data {
                            *sample = 0.0;
                        }
                        return;
                    }
                    let requested_drop = drop_old_frames_for_cb.swap(0, Ordering::Relaxed);
                    if requested_drop != 0 {
                        let skipped = consumer.skip(requested_drop);
                        if skipped != 0 {
                            dropped_old_frames_for_cb.fetch_add(skipped as u64, Ordering::Relaxed);
                            total_dropped_old_frames_for_cb
                                .fetch_add(skipped as u64, Ordering::Relaxed);
                            resampler.reset();
                        }
                    }
                    for frame in data.chunks_mut(chans) {
                        let (l, r) = if playback_started_for_cb.load(Ordering::Relaxed) {
                            if live_audio_should_rebuffer(consumer.occupied_len()) {
                                resampler.reset();
                                playback_started_for_cb.store(false, Ordering::Relaxed);
                                (0.0, 0.0)
                            } else {
                                resampler.next_frame(
                                    &mut consumer,
                                    &underruns_for_cb,
                                    &total_underruns_for_cb,
                                    &playback_started_for_cb,
                                )
                            }
                        } else {
                            resampler.reset();
                            (0.0, 0.0)
                        };
                        if chans == 1 {
                            frame[0] = 0.5 * (l + r);
                        } else {
                            frame[0] = l;
                            frame[1] = r;
                            for extra in &mut frame[2..] {
                                *extra = 0.0;
                            }
                        }
                    }
                },
                |err| log::warn!("cpal stream error: {err}"),
                None,
            )
            .map_err(|e| anyhow!("build_output_stream: {e}"))?;
        stream.play().map_err(|e| anyhow!("stream play: {e}"))?;

        log::info!(
            "audio: cpal sink ready, device={:?}, channels={}, output_rate={}, mix_rate={}",
            device.name().unwrap_or_else(|_| "<unknown>".into()),
            channels,
            output_sample_rate,
            MIX_SAMPLE_RATE
        );

        Ok(Self {
            producer,
            _stream: stream,
            playback_started,
            drop_old_frames,
            dropped_old_frames,
            total_dropped_old_frames,
            underruns,
            total_underruns,
            live_output_suspended,
            profile_callbacks,
            profile_callback_frames,
            profile_callback_device_cck,
            profile_enabled,
            generated_frames: 0,
            overruns: 0,
            total_overruns: 0,
            last_log: Instant::now(),
            prebuffer_frames,
        })
    }
}

struct CpalResampler {
    step: f64,
    phase: f64,
    current: (f32, f32),
    next: (f32, f32),
    primed: bool,
}

impl CpalResampler {
    fn new(output_sample_rate: u32) -> Self {
        Self {
            step: MIX_SAMPLE_RATE as f64 / output_sample_rate.max(1) as f64,
            phase: 0.0,
            current: (0.0, 0.0),
            next: (0.0, 0.0),
            primed: false,
        }
    }

    fn reset(&mut self) {
        self.phase = 0.0;
        self.current = (0.0, 0.0);
        self.next = (0.0, 0.0);
        self.primed = false;
    }

    fn next_frame(
        &mut self,
        consumer: &mut ringbuf::HeapCons<(f32, f32)>,
        underruns: &AtomicU64,
        total_underruns: &AtomicU64,
        playback_started: &AtomicBool,
    ) -> (f32, f32) {
        if !self.primed {
            let Some(current) = pop_live_audio_frame(consumer, underruns, total_underruns) else {
                self.stop_after_underrun(playback_started);
                return (0.0, 0.0);
            };
            let Some(next) = pop_live_audio_frame(consumer, underruns, total_underruns) else {
                self.stop_after_underrun(playback_started);
                return (0.0, 0.0);
            };
            self.current = current;
            self.next = next;
            self.primed = true;
        }

        let left = self.current.0 + (self.next.0 - self.current.0) * self.phase as f32;
        let right = self.current.1 + (self.next.1 - self.current.1) * self.phase as f32;

        self.phase += self.step;
        while self.phase >= 1.0 {
            self.current = self.next;
            let Some(next) = pop_live_audio_frame(consumer, underruns, total_underruns) else {
                self.stop_after_underrun(playback_started);
                return (0.0, 0.0);
            };
            self.next = next;
            self.phase -= 1.0;
        }

        (left, right)
    }

    fn stop_after_underrun(&mut self, playback_started: &AtomicBool) {
        self.reset();
        playback_started.store(false, Ordering::Relaxed);
    }
}

fn pop_live_audio_frame(
    consumer: &mut ringbuf::HeapCons<(f32, f32)>,
    underruns: &AtomicU64,
    total_underruns: &AtomicU64,
) -> Option<(f32, f32)> {
    consumer.try_pop().or_else(|| {
        underruns.fetch_add(1, Ordering::Relaxed);
        total_underruns.fetch_add(1, Ordering::Relaxed);
        None
    })
}

impl AudioSink for CpalSink {
    fn push(&mut self, left: f32, right: f32) {
        self.generated_frames = self.generated_frames.saturating_add(1);
        if !self.playback_started.load(Ordering::Relaxed)
            && self.producer.is_empty()
            && !sample_is_audible(left, right)
        {
            return;
        }

        if self.producer.try_push((left, right)).is_err() {
            self.overruns = self.overruns.saturating_add(1);
            self.total_overruns = self.total_overruns.saturating_add(1);
            self.request_stale_frame_drop();
        } else if !self.playback_started.load(Ordering::Relaxed)
            && self.producer.occupied_len() >= self.prebuffer_frames
        {
            self.playback_started.store(true, Ordering::Relaxed);
        } else if self.playback_started.load(Ordering::Relaxed) {
            self.request_stale_frame_drop();
        }

        // Periodically surface underrun counter so it's obvious when
        // the mixer can't keep up.
        if self.last_log.elapsed().as_secs() >= 1 {
            let underruns = self.underruns.swap(0, Ordering::Relaxed);
            let overruns = std::mem::take(&mut self.overruns);
            let dropped_old = self.dropped_old_frames.swap(0, Ordering::Relaxed);
            if underruns > 0 {
                log::warn!("audio: {underruns} cpal underrun frames in the last second");
            }
            if overruns > 0 {
                log::warn!(
                    "audio: {} cpal overrun frames dropped in the last second",
                    overruns
                );
            }
            if dropped_old > 0 {
                log::warn!("audio: skipped {dropped_old} stale cpal frames to bound live latency");
            }
            if self.profile_enabled {
                let callbacks = self.profile_callbacks.swap(0, Ordering::Relaxed);
                let callback_frames = self.profile_callback_frames.swap(0, Ordering::Relaxed);
                let callback_device_cck =
                    self.profile_callback_device_cck.swap(0, Ordering::Relaxed);
                let generated_frames = std::mem::take(&mut self.generated_frames);
                let avg_callback_device_cck = if callbacks == 0 {
                    0.0
                } else {
                    callback_device_cck as f64 / callbacks as f64
                };
                log::info!(
                    "audio profile: queue_depth={} generated_frames={} callback_underruns={} dropped_overrun_frames={} skipped_stale_frames={} callbacks={} callback_output_frames={} device_cck={} avg_device_cck_per_callback={:.1}",
                    self.producer.occupied_len(),
                    generated_frames,
                    underruns,
                    overruns,
                    dropped_old,
                    callbacks,
                    callback_frames,
                    callback_device_cck,
                    avg_callback_device_cck,
                );
            }
            self.last_log = Instant::now();
        }
    }

    fn flush(&mut self) {}

    fn set_live_output_suspended(&mut self, suspended: bool) {
        let previous = self
            .live_output_suspended
            .swap(suspended, Ordering::Relaxed);
        if previous == suspended {
            return;
        }
        // A deliberate host pause or modal filesystem operation can last
        // longer than the queued live-audio lead. Do not report those
        // callback silences as underruns when output resumes.
        self.underruns.store(0, Ordering::Relaxed);
        self.dropped_old_frames.store(0, Ordering::Relaxed);
        self.overruns = 0;
        self.last_log = Instant::now();
    }

    fn live_output_lead_seconds(&self) -> f64 {
        live_output_lead_seconds_for_state(
            self.playback_started.load(Ordering::Relaxed),
            self.producer.occupied_len(),
            CPAL_TARGET_BUFFER_FRAMES,
        )
    }

    fn runtime_status(&self) -> AudioRuntimeStatus {
        AudioRuntimeStatus {
            queue_depth_frames: self.producer.occupied_len(),
            output_lead_seconds: self.live_output_lead_seconds(),
            callback_underrun_frames: self.total_underruns.load(Ordering::Relaxed),
            dropped_overrun_frames: self.total_overruns,
            skipped_stale_frames: self.total_dropped_old_frames.load(Ordering::Relaxed),
        }
    }
}

impl CpalSink {
    fn request_stale_frame_drop(&self) {
        let stale = stale_live_audio_frames_to_skip(
            self.producer.occupied_len(),
            CPAL_TARGET_BUFFER_FRAMES,
            CPAL_STALE_DROP_THRESHOLD_FRAMES,
        );
        if stale != 0 {
            self.drop_old_frames.fetch_max(stale, Ordering::Relaxed);
        }
    }
}

// -----------------------------------------------------------------
// WavSink: dumps stereo f32 samples to a WAV file. Useful when the
// host doesn't have working audio output (CI, automated tests) or
// when you want to inspect the mixer output offline.
// -----------------------------------------------------------------

pub struct WavSink {
    writer: hound::WavWriter<BufWriter<File>>,
}

impl WavSink {
    pub fn new(path: &Path) -> Result<Self> {
        let spec = hound::WavSpec {
            channels: 2,
            sample_rate: MIX_SAMPLE_RATE,
            bits_per_sample: 32,
            sample_format: hound::SampleFormat::Float,
        };
        let writer = hound::WavWriter::create(path, spec)
            .map_err(|e| anyhow!("create WAV {}: {e}", path.display()))?;
        log::info!(
            "audio: WAV sink writing to {} (stereo f32 @ {} Hz)",
            path.display(),
            MIX_SAMPLE_RATE
        );
        Ok(Self { writer })
    }
}

impl AudioSink for WavSink {
    fn push(&mut self, left: f32, right: f32) {
        let _ = self.writer.write_sample(left);
        let _ = self.writer.write_sample(right);
    }

    fn flush(&mut self) {
        let _ = self.writer.flush();
    }
}

fn stale_live_audio_frames_to_skip(
    occupied_len: usize,
    target_len: usize,
    drop_threshold: usize,
) -> usize {
    if occupied_len <= drop_threshold {
        0
    } else {
        occupied_len.saturating_sub(target_len)
    }
}

fn sample_is_audible(left: f32, right: f32) -> bool {
    left != 0.0 || right != 0.0
}

fn live_output_lead_seconds_for_state(
    playback_started: bool,
    occupied_frames: usize,
    target_frames: usize,
) -> f64 {
    if !playback_started && occupied_frames == 0 {
        0.0
    } else if playback_started && occupied_frames < target_frames {
        let refill_frames = target_frames - occupied_frames;
        (target_frames + refill_frames) as f64 / MIX_SAMPLE_RATE as f64
    } else {
        target_frames as f64 / MIX_SAMPLE_RATE as f64
    }
}

fn live_audio_should_rebuffer(occupied_frames: usize) -> bool {
    occupied_frames <= CPAL_REBUFFER_THRESHOLD_FRAMES
}

fn callback_device_cck(output_frames: usize, output_sample_rate: u32) -> u64 {
    let rate = u64::from(output_sample_rate.max(1));
    (output_frames as u64)
        .saturating_mul(u64::from(PAL_PAULA_CLOCK_HZ))
        .div_ceil(rate)
}

#[cfg(test)]
mod tests {
    use super::{
        callback_device_cck, sample_is_audible, stale_live_audio_frames_to_skip, CpalResampler,
    };
    use ringbuf::traits::{Producer, Split};
    use ringbuf::HeapRb;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

    #[test]
    fn live_audio_backlog_uses_hysteresis_before_dropping_stale_frames() {
        assert_eq!(stale_live_audio_frames_to_skip(1024, 2048, 4096), 0);
        assert_eq!(stale_live_audio_frames_to_skip(2048, 2048, 4096), 0);
        assert_eq!(stale_live_audio_frames_to_skip(4096, 2048, 4096), 0);
        assert_eq!(stale_live_audio_frames_to_skip(8192, 2048, 4096), 6144);
    }

    #[test]
    fn live_audio_startup_drops_leading_silence() {
        assert!(!sample_is_audible(0.0, 0.0));
        assert!(sample_is_audible(0.0, 0.25));
        assert!(sample_is_audible(-0.25, 0.0));
    }

    #[test]
    fn live_audio_resampler_tracks_output_device_rate() {
        let resampler = CpalResampler::new(48_000);
        assert!((resampler.step - (44_100.0 / 48_000.0)).abs() < f64::EPSILON);
    }

    #[test]
    fn live_audio_resampler_stops_playback_after_underrun() {
        let rb = HeapRb::<(f32, f32)>::new(4);
        let (mut producer, mut consumer) = rb.split();
        producer.try_push((0.25, -0.25)).unwrap();
        let underruns = AtomicU64::new(0);
        let total_underruns = AtomicU64::new(0);
        let playback_started = AtomicBool::new(true);
        let mut resampler = CpalResampler::new(48_000);

        assert_eq!(
            resampler.next_frame(
                &mut consumer,
                &underruns,
                &total_underruns,
                &playback_started,
            ),
            (0.0, 0.0)
        );
        assert!(!playback_started.load(Ordering::Relaxed));
        assert_eq!(underruns.load(Ordering::Relaxed), 1);
        assert_eq!(total_underruns.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn live_audio_lead_starts_after_audible_queueing() {
        assert_eq!(
            super::live_output_lead_seconds_for_state(false, 0, 4096),
            0.0
        );
        assert!(super::live_output_lead_seconds_for_state(false, 1, 4096) > 0.0);
        assert!(super::live_output_lead_seconds_for_state(true, 0, 4096) > 0.0);
    }

    #[test]
    fn live_audio_lead_reports_started_queue_deficit_for_refill() {
        let target_seconds = 4096.0 / super::MIX_SAMPLE_RATE as f64;
        let underfilled_seconds = super::live_output_lead_seconds_for_state(true, 1024, 4096);
        let full_seconds = super::live_output_lead_seconds_for_state(true, 4096, 4096);
        let overfilled_seconds = super::live_output_lead_seconds_for_state(true, 8192, 4096);

        assert!(underfilled_seconds > target_seconds);
        assert_eq!(full_seconds, target_seconds);
        assert_eq!(overfilled_seconds, target_seconds);
    }

    #[test]
    fn live_audio_rebuffers_before_callback_queue_runs_dry() {
        assert!(super::live_audio_should_rebuffer(
            super::CPAL_REBUFFER_THRESHOLD_FRAMES
        ));
        assert!(!super::live_audio_should_rebuffer(
            super::CPAL_REBUFFER_THRESHOLD_FRAMES + 1
        ));
    }

    #[test]
    fn audio_profile_callback_cck_tracks_device_rate() {
        assert_eq!(callback_device_cck(48_000, 48_000), 3_546_895);
        assert_eq!(callback_device_cck(24_000, 48_000), 1_773_448);
    }
}
