// SPDX-License-Identifier: GPL-3.0-or-later

//! Video-with-audio capture to an AVI file. Started and stopped from the
//! window (host shortcut or the menu's recording item), so a capture can
//! bracket exactly the part of a session worth keeping.
//!
//! Video is encoded with ZMBV (the DOSBox capture codec): lossless,
//! zlib-deflated intra frames plus XOR-delta inter frames, which suits
//! Amiga output (large static regions, few colours) and needs no codec
//! dependency beyond the flate2 crate already used elsewhere. Audio is
//! plain 16-bit PCM stereo at the mixer rate. The result plays in
//! VLC/mpv/ffmpeg-based players; `ffmpeg -i capture.avi out.mp4`
//! transcodes it for anything else.
//!
//! Frames are pushed on emulated-frame boundaries and audio is the exact
//! mixer output for the same emulated interval, so A/V stay locked to the
//! emulated timeline regardless of host pacing (warp, stutter): the video
//! stream's rate/scale is patched at finish from the true ratio of frames
//! to audio samples.

use anyhow::{Context, Result};
use flate2::{Compress, Compression, FlushCompress, Status};
use std::fs::File;
use std::io::{BufWriter, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use crate::audio::MIX_SAMPLE_RATE;

// ZMBV stream parameters. 16x16 blocks match DOSBox; format 8 is the
// 32bpp (BGR0) pixel layout, the only one we emit.
const ZMBV_BLOCK: usize = 16;
const ZMBV_FORMAT_32BPP: u8 = 8;
/// One intra frame per emulated PAL second bounds seek granularity and
/// the damage a corrupt frame can do without bloating the file.
const KEYFRAME_INTERVAL: u32 = 50;

const AUDIO_BYTES_PER_FRAME: u32 = 4; // 16-bit stereo

// AVI flag/fourcc constants.
const AVIF_HASINDEX: u32 = 0x0000_0010;
const AVIF_ISINTERLEAVED: u32 = 0x0000_0100;
const AVIIF_KEYFRAME: u32 = 0x0000_0010;

/// Byte offsets of the header fields patched at finish. The header is
/// fixed-layout (see `build_header`), so these are compile-time facts;
/// `header_offsets_match_layout` asserts them against the builder.
const OFS_RIFF_SIZE: u64 = 4;
const OFS_US_PER_FRAME: u64 = 32;
const OFS_MAX_BYTES_PER_SEC: u64 = 36;
const OFS_TOTAL_FRAMES: u64 = 48;
const OFS_SUGGESTED_BUF: u64 = 60;
const OFS_VIDEO_SCALE: u64 = 128;
const OFS_VIDEO_RATE: u64 = 132;
const OFS_VIDEO_LENGTH: u64 = 140;
const OFS_VIDEO_SUGGESTED_BUF: u64 = 144;
const OFS_AUDIO_LENGTH: u64 = 264;
const OFS_AUDIO_SUGGESTED_BUF: u64 = 268;
const OFS_MOVI_SIZE: u64 = 316;
/// File position of the movi LIST header; chunk index offsets are
/// relative to the 'movi' fourcc at MOVI_POS + 8.
const MOVI_POS: u64 = 312;
const HEADER_LEN: usize = 324;

/// Pick a default filename for an interactive recording.
pub fn auto_filename() -> PathBuf {
    let n = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    PathBuf::from(format!("copperline-video-{n}.avi"))
}

struct IndexEntry {
    id: [u8; 4],
    flags: u32,
    offset: u32,
    size: u32,
}

pub struct VideoRecorder {
    out: BufWriter<File>,
    path: PathBuf,
    /// Bytes written so far; tracked manually so chunk positions never
    /// need a buffer-flushing stream_position() round trip.
    pos: u64,
    zmbv: ZmbvEncoder,
    /// Reused encoded-frame scratch buffer.
    payload: Vec<u8>,
    /// Interleaved little-endian 16-bit stereo samples awaiting the next
    /// chunk write.
    pending_audio: Vec<u8>,
    index: Vec<IndexEntry>,
    video_frames: u32,
    audio_sample_frames: u32,
    max_chunk: u32,
    finished: bool,
}

impl VideoRecorder {
    /// Open `path` and write the AVI header for a `width` x `height`
    /// RGBA stream. The header's timing fields are placeholders until
    /// [`finish`](Self::finish) patches them.
    pub fn create(path: &Path, width: usize, height: usize) -> Result<Self> {
        let file =
            File::create(path).with_context(|| format!("creating recording {}", path.display()))?;
        let mut out = BufWriter::new(file);
        let header = build_header(width as u32, height as u32);
        out.write_all(&header)
            .with_context(|| format!("writing AVI header to {}", path.display()))?;
        Ok(Self {
            out,
            path: path.to_path_buf(),
            pos: header.len() as u64,
            zmbv: ZmbvEncoder::new(width, height),
            payload: Vec::new(),
            pending_audio: Vec::new(),
            index: Vec::new(),
            video_frames: 0,
            audio_sample_frames: 0,
            max_chunk: 0,
            finished: false,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Captured length so far, from the audio clock (exact emulated time).
    pub fn recorded_seconds(&self) -> f64 {
        f64::from(self.audio_sample_frames) / f64::from(MIX_SAMPLE_RATE)
    }

    /// Queue mixer output ([-1, 1] stereo) for the next audio chunk.
    pub fn push_audio(&mut self, samples: &[(f32, f32)]) {
        for &(left, right) in samples {
            for ch in [left, right] {
                let q = (ch.clamp(-1.0, 1.0) * 32767.0).round() as i16;
                self.pending_audio.extend_from_slice(&q.to_le_bytes());
            }
        }
    }

    /// Encode and write one video frame (RGBA pixels, row-major,
    /// width*height as passed to [`create`](Self::create)), followed by
    /// the audio queued since the previous frame.
    pub fn push_frame(&mut self, fb: &[u32]) -> Result<()> {
        let mut payload = std::mem::take(&mut self.payload);
        let intra = self.zmbv.encode(fb, &mut payload)?;
        let flags = if intra { AVIIF_KEYFRAME } else { 0 };
        self.write_chunk(*b"00dc", &payload, flags)?;
        self.payload = payload;
        self.video_frames += 1;
        self.flush_audio_chunk()?;
        Ok(())
    }

    /// Write the index, patch the header timing/size fields, and flush.
    /// The video rate is derived from the audio sample count so A/V sync
    /// matches the emulated timeline exactly (a nominal "50 fps" label
    /// would drift against PAL's 49.92 Hz).
    pub fn finish(&mut self) -> Result<()> {
        if self.finished {
            return Ok(());
        }
        self.finished = true;
        self.flush_audio_chunk()?;

        let movi_size = (self.pos - (MOVI_POS + 8)) as u32;
        self.write_index()?;
        let riff_size = (self.pos - 8) as u32;
        let (rate, scale) = video_timebase(self.video_frames, self.audio_sample_frames);
        let us_per_frame = (1_000_000u64 * u64::from(scale) / u64::from(rate.max(1))) as u32;
        let bytes_per_sec = if self.recorded_seconds() > 0.0 {
            (self.pos as f64 / self.recorded_seconds()) as u32
        } else {
            0
        };

        let patches: [(u64, u32); 12] = [
            (OFS_RIFF_SIZE, riff_size),
            (OFS_US_PER_FRAME, us_per_frame),
            (OFS_MAX_BYTES_PER_SEC, bytes_per_sec),
            (OFS_TOTAL_FRAMES, self.video_frames),
            (OFS_SUGGESTED_BUF, self.max_chunk + 8),
            (OFS_VIDEO_SCALE, scale),
            (OFS_VIDEO_RATE, rate),
            (OFS_VIDEO_LENGTH, self.video_frames),
            (OFS_VIDEO_SUGGESTED_BUF, self.max_chunk + 8),
            (OFS_AUDIO_LENGTH, self.audio_sample_frames),
            (OFS_AUDIO_SUGGESTED_BUF, self.max_chunk + 8),
            (OFS_MOVI_SIZE, movi_size),
        ];
        for (ofs, value) in patches {
            self.out.seek(SeekFrom::Start(ofs))?;
            self.out.write_all(&value.to_le_bytes())?;
        }
        self.out.flush().context("flushing recording")?;
        Ok(())
    }

    fn flush_audio_chunk(&mut self) -> Result<()> {
        if self.pending_audio.is_empty() {
            return Ok(());
        }
        let data = std::mem::take(&mut self.pending_audio);
        self.audio_sample_frames += data.len() as u32 / AUDIO_BYTES_PER_FRAME;
        self.write_chunk(*b"01wb", &data, AVIIF_KEYFRAME)?;
        Ok(())
    }

    fn write_chunk(&mut self, id: [u8; 4], data: &[u8], flags: u32) -> Result<()> {
        self.index.push(IndexEntry {
            id,
            flags,
            offset: (self.pos - (MOVI_POS + 8)) as u32,
            size: data.len() as u32,
        });
        self.out.write_all(&id)?;
        self.out.write_all(&(data.len() as u32).to_le_bytes())?;
        self.out.write_all(data)?;
        self.pos += 8 + data.len() as u64;
        if data.len() % 2 == 1 {
            self.out.write_all(&[0])?;
            self.pos += 1;
        }
        self.max_chunk = self.max_chunk.max(data.len() as u32);
        Ok(())
    }

    fn write_index(&mut self) -> Result<()> {
        self.out.write_all(b"idx1")?;
        self.out
            .write_all(&((self.index.len() * 16) as u32).to_le_bytes())?;
        for entry in &self.index {
            self.out.write_all(&entry.id)?;
            self.out.write_all(&entry.flags.to_le_bytes())?;
            self.out.write_all(&entry.offset.to_le_bytes())?;
            self.out.write_all(&entry.size.to_le_bytes())?;
        }
        self.pos += 8 + (self.index.len() * 16) as u64;
        Ok(())
    }
}

impl Drop for VideoRecorder {
    /// Best-effort finalize so quitting mid-recording still leaves a
    /// playable file.
    fn drop(&mut self) {
        if !self.finished {
            if let Err(e) = self.finish() {
                log::warn!("finalizing recording on drop failed: {e:#}");
            }
        }
    }
}

/// The AVI video timebase (rate/scale) that makes `frames` span exactly
/// the duration of `audio_frames` mixer samples.
fn video_timebase(frames: u32, audio_frames: u32) -> (u32, u32) {
    if frames == 0 || audio_frames == 0 {
        return (50, 1);
    }
    let mut rate = u64::from(frames) * u64::from(MIX_SAMPLE_RATE);
    let mut scale = u64::from(audio_frames);
    let g = gcd(rate, scale);
    rate /= g;
    scale /= g;
    while rate > u64::from(u32::MAX) || scale > u64::from(u32::MAX) {
        rate /= 2;
        scale /= 2;
    }
    (rate.max(1) as u32, scale.max(1) as u32)
}

fn gcd(mut a: u64, mut b: u64) -> u64 {
    while b != 0 {
        (a, b) = (b, a % b);
    }
    a
}

/// The fixed-layout RIFF AVI header up to and including the movi LIST
/// header. Size/timing fields are zero placeholders patched by `finish`.
fn build_header(width: u32, height: u32) -> Vec<u8> {
    let mut h = Vec::with_capacity(HEADER_LEN);
    let w32 = |h: &mut Vec<u8>, v: u32| h.extend_from_slice(&v.to_le_bytes());
    let w16 = |h: &mut Vec<u8>, v: u16| h.extend_from_slice(&v.to_le_bytes());

    h.extend_from_slice(b"RIFF");
    w32(&mut h, 0); // riff size (patched)
    h.extend_from_slice(b"AVI ");

    h.extend_from_slice(b"LIST");
    w32(&mut h, 292); // hdrl contents
    h.extend_from_slice(b"hdrl");

    h.extend_from_slice(b"avih");
    w32(&mut h, 56);
    w32(&mut h, 0); // dwMicroSecPerFrame (patched)
    w32(&mut h, 0); // dwMaxBytesPerSec (patched)
    w32(&mut h, 0); // dwPaddingGranularity
    w32(&mut h, AVIF_HASINDEX | AVIF_ISINTERLEAVED);
    w32(&mut h, 0); // dwTotalFrames (patched)
    w32(&mut h, 0); // dwInitialFrames
    w32(&mut h, 2); // dwStreams
    w32(&mut h, 0); // dwSuggestedBufferSize (patched)
    w32(&mut h, width);
    w32(&mut h, height);
    for _ in 0..4 {
        w32(&mut h, 0); // dwReserved
    }

    // Video stream list.
    h.extend_from_slice(b"LIST");
    w32(&mut h, 116);
    h.extend_from_slice(b"strl");
    h.extend_from_slice(b"strh");
    w32(&mut h, 56);
    h.extend_from_slice(b"vids");
    h.extend_from_slice(b"ZMBV");
    w32(&mut h, 0); // dwFlags
    w16(&mut h, 0); // wPriority
    w16(&mut h, 0); // wLanguage
    w32(&mut h, 0); // dwInitialFrames
    w32(&mut h, 0); // dwScale (patched)
    w32(&mut h, 0); // dwRate (patched)
    w32(&mut h, 0); // dwStart
    w32(&mut h, 0); // dwLength (patched)
    w32(&mut h, 0); // dwSuggestedBufferSize (patched)
    w32(&mut h, 10_000); // dwQuality
    w32(&mut h, 0); // dwSampleSize
    w16(&mut h, 0); // rcFrame left
    w16(&mut h, 0); // rcFrame top
    w16(&mut h, width as u16);
    w16(&mut h, height as u16);
    h.extend_from_slice(b"strf");
    w32(&mut h, 40); // BITMAPINFOHEADER
    w32(&mut h, 40); // biSize
    w32(&mut h, width);
    w32(&mut h, height);
    w16(&mut h, 1); // biPlanes
    w16(&mut h, 24); // biBitCount (nominal; ZMBV carries its own format)
    h.extend_from_slice(b"ZMBV"); // biCompression
    w32(&mut h, width * height * 4); // biSizeImage
    w32(&mut h, 0); // biXPelsPerMeter
    w32(&mut h, 0); // biYPelsPerMeter
    w32(&mut h, 0); // biClrUsed
    w32(&mut h, 0); // biClrImportant

    // Audio stream list.
    h.extend_from_slice(b"LIST");
    w32(&mut h, 92);
    h.extend_from_slice(b"strl");
    h.extend_from_slice(b"strh");
    w32(&mut h, 56);
    h.extend_from_slice(b"auds");
    w32(&mut h, 0); // fccHandler
    w32(&mut h, 0); // dwFlags
    w16(&mut h, 0); // wPriority
    w16(&mut h, 0); // wLanguage
    w32(&mut h, 0); // dwInitialFrames
    w32(&mut h, 1); // dwScale
    w32(&mut h, MIX_SAMPLE_RATE); // dwRate
    w32(&mut h, 0); // dwStart
    w32(&mut h, 0); // dwLength (patched)
    w32(&mut h, 0); // dwSuggestedBufferSize (patched)
    w32(&mut h, 10_000); // dwQuality
    w32(&mut h, AUDIO_BYTES_PER_FRAME); // dwSampleSize
    w32(&mut h, 0); // rcFrame (unused for audio)
    w32(&mut h, 0);
    h.extend_from_slice(b"strf");
    w32(&mut h, 16); // PCMWAVEFORMAT
    w16(&mut h, 1); // wFormatTag = PCM
    w16(&mut h, 2); // nChannels
    w32(&mut h, MIX_SAMPLE_RATE); // nSamplesPerSec
    w32(&mut h, MIX_SAMPLE_RATE * AUDIO_BYTES_PER_FRAME); // nAvgBytesPerSec
    w16(&mut h, AUDIO_BYTES_PER_FRAME as u16); // nBlockAlign
    w16(&mut h, 16); // wBitsPerSample

    h.extend_from_slice(b"LIST");
    w32(&mut h, 0); // movi size (patched)
    h.extend_from_slice(b"movi");

    debug_assert_eq!(h.len(), HEADER_LEN);
    h
}

// ---------------------------------------------------------------------
// ZMBV encoder
// ---------------------------------------------------------------------

/// ZMBV ("Zip Motion Blocks Video") encoder, 32bpp only. Intra frames
/// carry the whole picture; inter frames carry a per-16x16-block change
/// table plus XOR deltas against the previous frame (always with motion
/// vector (0,0): a search rarely pays off on Amiga output, and decoders
/// treat the zero vector as plain frame differencing). The deflate
/// stream is reset on intra frames and continues across the inter frames
/// that follow, as the format requires.
struct ZmbvEncoder {
    width: usize,
    height: usize,
    /// Current and previous frames as BGR0 bytes (the ZMBV 32bpp pixel
    /// layout).
    cur: Vec<u8>,
    prev: Vec<u8>,
    /// Uncompressed inter-frame payload scratch (block table + deltas).
    delta: Vec<u8>,
    comp: Compress,
    frames_since_intra: Option<u32>,
}

impl ZmbvEncoder {
    fn new(width: usize, height: usize) -> Self {
        Self {
            width,
            height,
            cur: vec![0; width * height * 4],
            prev: vec![0; width * height * 4],
            delta: Vec::new(),
            comp: Compress::new(Compression::fast(), true),
            frames_since_intra: None,
        }
    }

    /// Encode `fb` (RGBA pixels) into `out`, returning whether this is an
    /// intra (key) frame.
    fn encode(&mut self, fb: &[u32], out: &mut Vec<u8>) -> Result<bool> {
        anyhow::ensure!(
            fb.len() == self.width * self.height,
            "recorder frame size mismatch: got {} pixels, expected {}x{}",
            fb.len(),
            self.width,
            self.height
        );
        out.clear();
        for (dst, &px) in self.cur.chunks_exact_mut(4).zip(fb) {
            let [r, g, b, _] = px.to_le_bytes();
            dst.copy_from_slice(&[b, g, r, 0]);
        }

        let intra = !matches!(self.frames_since_intra, Some(n) if n < KEYFRAME_INTERVAL);
        if intra {
            out.extend_from_slice(&[
                0x01, // intra flag
                0,    // major version
                1,    // minor version
                1,    // compression: zlib
                ZMBV_FORMAT_32BPP,
                ZMBV_BLOCK as u8,
                ZMBV_BLOCK as u8,
            ]);
            self.comp.reset();
            deflate_sync(&mut self.comp, &self.cur, out)?;
            self.frames_since_intra = Some(1);
        } else {
            out.push(0x00);
            self.build_delta();
            deflate_sync(&mut self.comp, &self.delta, out)?;
            self.frames_since_intra = self.frames_since_intra.map(|n| n + 1);
        }
        std::mem::swap(&mut self.cur, &mut self.prev);
        Ok(intra)
    }

    /// Fill `self.delta` with the inter-frame payload: a 2-byte entry per
    /// block (bit 0 of the first byte = "block changed", motion vector
    /// zero), padded to a 4-byte boundary, then the XOR data of each
    /// changed block in scan order. Edge blocks are clipped to the frame.
    fn build_delta(&mut self) {
        let bw = self.width.div_ceil(ZMBV_BLOCK);
        let bh = self.height.div_ceil(ZMBV_BLOCK);
        let table_len = (bw * bh * 2 + 3) & !3;
        self.delta.clear();
        self.delta.resize(table_len, 0);
        let stride = self.width * 4;
        let mut block = 0;
        for by in 0..bh {
            let y0 = by * ZMBV_BLOCK;
            let rows = ZMBV_BLOCK.min(self.height - y0);
            for bx in 0..bw {
                let x0 = bx * ZMBV_BLOCK * 4;
                let line = (ZMBV_BLOCK * 4).min(stride - x0);
                let changed = (0..rows).any(|row| {
                    let ofs = (y0 + row) * stride + x0;
                    self.cur[ofs..ofs + line] != self.prev[ofs..ofs + line]
                });
                if changed {
                    self.delta[block * 2] = 0x01;
                    for row in 0..rows {
                        let ofs = (y0 + row) * stride + x0;
                        let cur = &self.cur[ofs..ofs + line];
                        let prev = &self.prev[ofs..ofs + line];
                        self.delta.extend(cur.iter().zip(prev).map(|(c, p)| c ^ p));
                    }
                }
                block += 1;
            }
        }
    }
}

/// Deflate `input` onto `out`, ending with a zlib sync flush so the
/// frame's compressed data is self-contained while the stream carries on
/// into the next frame.
fn deflate_sync(comp: &mut Compress, mut input: &[u8], out: &mut Vec<u8>) -> Result<()> {
    loop {
        if out.capacity() == out.len() {
            out.reserve(16 * 1024);
        }
        let flush = if input.is_empty() {
            FlushCompress::Sync
        } else {
            FlushCompress::None
        };
        let before = comp.total_in();
        let status = comp
            .compress_vec(input, out, flush)
            .context("ZMBV deflate")?;
        anyhow::ensure!(
            status != Status::StreamEnd,
            "ZMBV deflate stream ended unexpectedly"
        );
        input = &input[(comp.total_in() - before) as usize..];
        // A sync flush is complete once the compressor stops filling the
        // output buffer.
        if input.is_empty() && flush == FlushCompress::Sync && out.len() < out.capacity() {
            return Ok(());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::{Decompress, FlushDecompress};

    /// Inflate one frame's worth of sync-flushed data from `input`.
    fn inflate_frame(inflater: &mut Decompress, input: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(64 * 1024);
        let mut pos = 0;
        loop {
            if out.capacity() == out.len() {
                out.reserve(64 * 1024);
            }
            let before = inflater.total_in();
            inflater
                .decompress_vec(&input[pos..], &mut out, FlushDecompress::None)
                .expect("inflate");
            pos += (inflater.total_in() - before) as usize;
            if pos == input.len() && out.len() < out.capacity() {
                return out;
            }
        }
    }

    /// Reference ZMBV decoder for round-trip tests: returns the BGR0
    /// frame after applying `payload` on top of `prev`.
    fn zmbv_decode(
        payload: &[u8],
        prev: &[u8],
        inflater: &mut Option<Decompress>,
        width: usize,
        height: usize,
    ) -> Vec<u8> {
        if payload[0] & 1 == 1 {
            assert_eq!(
                &payload[1..7],
                &[0, 1, 1, ZMBV_FORMAT_32BPP, 16, 16],
                "intra header"
            );
            let mut z = Decompress::new(true);
            let frame = inflate_frame(&mut z, &payload[7..]);
            *inflater = Some(z);
            assert_eq!(frame.len(), width * height * 4);
            return frame;
        }
        let z = inflater.as_mut().expect("inter frame before intra");
        let raw = inflate_frame(z, &payload[1..]);
        let bw = width.div_ceil(16);
        let bh = height.div_ceil(16);
        let table_len = (bw * bh * 2 + 3) & !3;
        let mut frame = prev.to_vec();
        let mut data = table_len;
        let stride = width * 4;
        for by in 0..bh {
            for bx in 0..bw {
                let entry = raw[(by * bw + bx) * 2];
                assert_eq!(raw[(by * bw + bx) * 2 + 1], 0, "vertical motion");
                assert_eq!(entry & !1, 0, "horizontal motion");
                if entry & 1 == 0 {
                    continue;
                }
                let y0 = by * 16;
                let rows = 16.min(height - y0);
                let x0 = bx * 16 * 4;
                let line = (16 * 4).min(stride - x0);
                for row in 0..rows {
                    let ofs = (y0 + row) * stride + x0;
                    for i in 0..line {
                        frame[ofs + i] ^= raw[data];
                        data += 1;
                    }
                }
            }
        }
        assert_eq!(data, raw.len(), "all delta bytes consumed");
        frame
    }

    fn bgr0_of(fb: &[u32]) -> Vec<u8> {
        fb.iter()
            .flat_map(|px| {
                let [r, g, b, _] = px.to_le_bytes();
                [b, g, r, 0]
            })
            .collect()
    }

    /// RGBA pixel value (memory order R,G,B,A) from a seed.
    fn pixel(seed: u32) -> u32 {
        u32::from_le_bytes([(seed * 7) as u8, (seed * 13) as u8, (seed * 29) as u8, 0xFF])
    }

    #[test]
    fn zmbv_round_trips_intra_and_delta_frames() {
        // Odd dimensions exercise the clipped right/bottom edge blocks.
        let (w, h) = (43, 21);
        let mut enc = ZmbvEncoder::new(w, h);
        let mut payload = Vec::new();

        let frame0: Vec<u32> = (0..w * h).map(|i| pixel(i as u32)).collect();
        assert!(enc.encode(&frame0, &mut payload).unwrap());
        let mut inflater = None;
        let decoded0 = zmbv_decode(&payload, &[], &mut inflater, w, h);
        assert_eq!(decoded0, bgr0_of(&frame0));

        // Change one pixel mid-frame and one in the clipped corner block.
        let mut frame1 = frame0.clone();
        frame1[5 * w + 17] = pixel(9999);
        frame1[w * h - 1] = pixel(4242);
        assert!(!enc.encode(&frame1, &mut payload).unwrap());
        let decoded1 = zmbv_decode(&payload, &decoded0, &mut inflater, w, h);
        assert_eq!(decoded1, bgr0_of(&frame1));

        // An unchanged frame still decodes (all blocks skipped).
        assert!(!enc.encode(&frame1, &mut payload).unwrap());
        let decoded2 = zmbv_decode(&payload, &decoded1, &mut inflater, w, h);
        assert_eq!(decoded2, bgr0_of(&frame1));
    }

    #[test]
    fn zmbv_emits_keyframes_on_the_interval() {
        let (w, h) = (16, 16);
        let mut enc = ZmbvEncoder::new(w, h);
        let mut payload = Vec::new();
        let frame: Vec<u32> = vec![pixel(3); w * h];
        let mut intra_at = Vec::new();
        for i in 0..(KEYFRAME_INTERVAL * 2 + 2) {
            if enc.encode(&frame, &mut payload).unwrap() {
                intra_at.push(i);
            }
        }
        assert_eq!(intra_at, vec![0, KEYFRAME_INTERVAL, KEYFRAME_INTERVAL * 2]);
    }

    #[test]
    fn header_offsets_match_layout() {
        let h = build_header(716, 537);
        assert_eq!(h.len(), HEADER_LEN);
        let fourcc = |ofs: usize| &h[ofs..ofs + 4];
        assert_eq!(fourcc(0), b"RIFF");
        assert_eq!(fourcc(8), b"AVI ");
        assert_eq!(fourcc(24), b"avih");
        assert_eq!(fourcc(OFS_US_PER_FRAME as usize - 8), b"avih");
        assert_eq!(fourcc(100), b"strh");
        assert_eq!(fourcc(108), b"vids");
        assert_eq!(fourcc(112), b"ZMBV");
        assert_eq!(fourcc(188), b"ZMBV"); // biCompression
        assert_eq!(fourcc(232), b"auds");
        assert_eq!(fourcc(MOVI_POS as usize), b"LIST");
        assert_eq!(fourcc(MOVI_POS as usize + 8), b"movi");
        // Patched fields start zeroed.
        for ofs in [
            OFS_RIFF_SIZE,
            OFS_TOTAL_FRAMES,
            OFS_VIDEO_SCALE,
            OFS_VIDEO_RATE,
            OFS_VIDEO_LENGTH,
            OFS_AUDIO_LENGTH,
            OFS_MOVI_SIZE,
        ] {
            let ofs = ofs as usize;
            assert_eq!(&h[ofs..ofs + 4], &[0, 0, 0, 0], "offset {ofs}");
        }
        // Dimensions land in avih and the bitmap header.
        assert_eq!(&h[64..68], &716u32.to_le_bytes());
        assert_eq!(&h[68..72], &537u32.to_le_bytes());
        assert_eq!(&h[176..180], &716u32.to_le_bytes());
        assert_eq!(&h[180..184], &537u32.to_le_bytes());
    }

    #[test]
    fn video_timebase_locks_to_audio_clock() {
        // Exactly 50 fps when audio says so.
        assert_eq!(video_timebase(100, 2 * MIX_SAMPLE_RATE), (50, 1));
        // PAL-ish: 4992 frames per 100 s of audio = 49.92 fps.
        let (rate, scale) = video_timebase(4992, 100 * MIX_SAMPLE_RATE);
        let fps = f64::from(rate) / f64::from(scale);
        assert!((fps - 49.92).abs() < 1e-9, "fps {fps}");
        // No audio: nominal PAL fallback.
        assert_eq!(video_timebase(123, 0), (50, 1));
    }

    #[test]
    fn recorder_writes_a_well_formed_avi() {
        let (w, h) = (40, 18);
        let dir = std::env::temp_dir();
        let path = dir.join(format!(
            "copperline-recorder-test-{}.avi",
            std::process::id()
        ));
        let mut rec = VideoRecorder::create(&path, w, h).unwrap();

        let frame0: Vec<u32> = (0..w * h).map(|i| pixel(i as u32)).collect();
        let mut frame1 = frame0.clone();
        frame1[0] = pixel(777);
        // 882 mixer samples per 50 Hz frame.
        let audio: Vec<(f32, f32)> = (0..882).map(|i| (i as f32 / 882.0, -0.5)).collect();
        rec.push_audio(&audio);
        rec.push_frame(&frame0).unwrap();
        rec.push_audio(&audio);
        rec.push_frame(&frame1).unwrap();
        rec.push_audio(&audio);
        rec.finish().unwrap();
        assert!((rec.recorded_seconds() - 3.0 * 882.0 / 44_100.0).abs() < 1e-9);
        drop(rec);

        let data = std::fs::read(&path).unwrap();
        std::fs::remove_file(&path).unwrap();
        assert_eq!(&data[0..4], b"RIFF");
        let riff_size = u32::from_le_bytes(data[4..8].try_into().unwrap());
        assert_eq!(riff_size as usize, data.len() - 8);

        // Two video frames against three audio pushes of 882 samples.
        let frames = u32::from_le_bytes(
            data[OFS_TOTAL_FRAMES as usize..OFS_TOTAL_FRAMES as usize + 4]
                .try_into()
                .unwrap(),
        );
        assert_eq!(frames, 2);
        let audio_len = u32::from_le_bytes(
            data[OFS_AUDIO_LENGTH as usize..OFS_AUDIO_LENGTH as usize + 4]
                .try_into()
                .unwrap(),
        );
        assert_eq!(audio_len, 3 * 882);
        let rate = u32::from_le_bytes(
            data[OFS_VIDEO_RATE as usize..OFS_VIDEO_RATE as usize + 4]
                .try_into()
                .unwrap(),
        );
        let scale = u32::from_le_bytes(
            data[OFS_VIDEO_SCALE as usize..OFS_VIDEO_SCALE as usize + 4]
                .try_into()
                .unwrap(),
        );
        // 2 frames / (2646/44100 s) = 100/3 fps.
        assert_eq!((rate, scale), (100, 3));

        // First movi chunk is the keyframe video chunk; walk the chunks.
        let movi = MOVI_POS as usize;
        assert_eq!(&data[movi + 8..movi + 12], b"movi");
        let mut pos = movi + 12;
        let mut chunk_ids = Vec::new();
        while &data[pos..pos + 4] != b"idx1" {
            let id = &data[pos..pos + 4];
            let size = u32::from_le_bytes(data[pos + 4..pos + 8].try_into().unwrap()) as usize;
            chunk_ids.push(id.to_vec());
            pos += 8 + size + (size & 1);
        }
        assert_eq!(
            chunk_ids,
            vec![
                b"00dc".to_vec(),
                b"01wb".to_vec(),
                b"00dc".to_vec(),
                b"01wb".to_vec(),
                b"01wb".to_vec(),
            ]
        );
        let movi_size = u32::from_le_bytes(
            data[OFS_MOVI_SIZE as usize..OFS_MOVI_SIZE as usize + 4]
                .try_into()
                .unwrap(),
        );
        assert_eq!(movi + 8 + movi_size as usize, pos);

        // Index: one entry per chunk, video keyframe flagged, offsets
        // relative to the 'movi' fourcc.
        let count = u32::from_le_bytes(data[pos + 4..pos + 8].try_into().unwrap()) / 16;
        assert_eq!(count, 5);
        let e0 = pos + 8;
        assert_eq!(&data[e0..e0 + 4], b"00dc");
        let flags = u32::from_le_bytes(data[e0 + 4..e0 + 8].try_into().unwrap());
        assert_eq!(flags, AVIIF_KEYFRAME);
        let offset = u32::from_le_bytes(data[e0 + 8..e0 + 12].try_into().unwrap());
        assert_eq!(offset, 4);

        // The first video chunk decodes back to the first frame.
        let size = u32::from_le_bytes(data[movi + 16..movi + 20].try_into().unwrap()) as usize;
        let payload = &data[movi + 20..movi + 20 + size];
        let mut inflater = None;
        let decoded = zmbv_decode(payload, &[], &mut inflater, w, h);
        assert_eq!(decoded, bgr0_of(&frame0));

        // The first audio chunk carries the quantized mixer samples.
        let a_pos = movi + 20 + size + (size & 1);
        assert_eq!(&data[a_pos..a_pos + 4], b"01wb");
        let a_size = u32::from_le_bytes(data[a_pos + 4..a_pos + 8].try_into().unwrap()) as usize;
        assert_eq!(a_size, 882 * 4);
        let right = i16::from_le_bytes(data[a_pos + 10..a_pos + 12].try_into().unwrap());
        assert_eq!(right, (-0.5f32 * 32767.0).round() as i16);
    }
}
