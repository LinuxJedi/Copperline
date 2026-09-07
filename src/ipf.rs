// SPDX-License-Identifier: GPL-3.0-or-later

//! IPF (Interchangeable Preservation Format) floppy images.
//!
//! IPF is the SPS/CAPS preservation format: rather than sector contents it
//! stores the *encoded* track -- every MFM cell the head passes over, sector
//! headers, sync marks, gaps and all -- which is why it preserves the custom
//! trackloaders and copy protections that an ADF cannot express.
//!
//! # What the format stores
//!
//! A file is a sequence of chunks, each a four-character name, a big-endian
//! total length, and a CRC-32 of the chunk taken with the CRC field zeroed:
//!
//! - `CAPS` -- the file signature, a bare header.
//! - `INFO` -- media type, the encoder that produced the file, and the
//!   cylinder/side range covered.
//! - `IMGE` -- one per track, describing its geometry: how many bits the
//!   revolution holds, how many of them are block data and how many gap,
//!   where the first block starts relative to the index, and a *data key*
//!   naming the `DATA` chunk that carries the content.
//! - `DATA` -- the content for one track. Uniquely among the chunks, its
//!   payload continues past the length in its header: a data area of
//!   `dataSize` further bytes follows, holding the block descriptors and then
//!   the gap and data streams they point at.
//!
//! A track is a series of *blocks* -- an AmigaDOS sector, or one whole custom
//! track for a trackloader that does its own thing. Each block is a data
//! stream of `block_bits` encoded MFM bits followed by a gap of `gap_bits`.
//!
//! # How a stream becomes cells
//!
//! Both stream kinds are a run of samples terminated by a nul byte. A sample
//! opens with a type byte whose low five bits are the type and whose top three
//! say how many big-endian bytes of length follow.
//!
//! In a data stream the types are sync (1), data (2), gap (3), raw (4) and
//! weak (5). Sync and raw are already-encoded MFM written through untouched --
//! that is how an address mark like `4489 4489` keeps its illegal clocking --
//! while data and gap hold decoded bytes that this decoder MFM-encodes,
//! setting the clock bit only between two zero data bits. So a sample's
//! contribution is its bit count for raw kinds and twice that for encoded
//! kinds, and the sum across the stream is exactly the `block_bits` the
//! descriptor promised, which [`decode`] checks track by track.
//!
//! Gap streams are richer, because a gap is where the write splice falls and
//! its length depends on how fast the drive that wrote it was turning. A gap
//! may be filled from a single repeated byte, or from a forward stream, a
//! backward stream, or both, each with an optional *loop sample* that stretches
//! to take up whatever slack is left. Two streams meet in the middle of the
//! gap, which is where a real splice sits.
//!
//! # Why a direct parser
//!
//! The reference decoder is SPS's closed-source `capsimg` shared library.
//! Copperline decodes IPF itself so that a build which says it reads IPF
//! actually does, on every platform, with nothing to download -- the same
//! reasoning that links FluxBridge into [`crate::fluxbridge`] rather than
//! dlopen-ing it.
//!
//! # What comes out
//!
//! One whole revolution per track as packed MFM, in the same shape a flux
//! capture already takes in [`crate::floppy`] -- so the rotation, PLL and
//! sync-word machinery reads an IPF with no special case in the hot path.
//! The revolution is rotated to start at the index, as a capture does, using
//! the start position the `IMGE` records.

use anyhow::{bail, ensure, Context, Result};
use flate2::Crc;
use log::warn;

const CAPS_SIGNATURE: &[u8; 4] = b"CAPS";
const CHUNK_HEADER_LEN: usize = 12;
const INFO_BODY_LEN: usize = 84;
const IMGE_BODY_LEN: usize = 68;
const DATA_BODY_LEN: usize = 16;
const BLOCK_DESCRIPTOR_LEN: usize = 32;

/// Cylinder 83 side 1 is the highest track the format describes for Amiga
/// media, matching the 168 slots the SCP loader keeps.
const MAX_TRACKS: usize = 2 * 84;

/// A revolution longer than this is a corrupt descriptor rather than a disk:
/// a 300 RPM DD revolution is close to 100,000 cells.
const MAX_TRACK_BITS: usize = 1_000_000;

// INFO encoder identifiers.
const ENCODER_CAPS: u32 = 1;

// INFO media types.
const MEDIA_FLOPPY: u32 = 1;

// IMGE signal types: the cell rate the track was recorded at.
const SIGNAL_2US_CELLS: u32 = 1;

// IMGE cell-density models. Anything past `AUTO` varies the cell rate across
// the track to defeat copiers, and needs a per-protection timing model.
const DENSITY_NOISE: u32 = 1;
const DENSITY_AUTO: u32 = 2;
// The mastered cell-rate profiles: which blocks of the track were written
// with cells longer or shorter than nominal, and by how much, so a loader
// timing them can tell an original from a copy. Modelled after the CAPS
// library's `GenerateCLA`/`GenerateSLA`/`GenerateABA` family, which scales
// each byte's cell time in per-mille of nominal.
const DENSITY_COPYLOCK_AMIGA: u32 = 3;
const DENSITY_COPYLOCK_AMIGA_NEW: u32 = 4;
const DENSITY_COPYLOCK_ST: u32 = 5;
const DENSITY_SPEEDLOCK_AMIGA: u32 = 6;
const DENSITY_SPEEDLOCK_AMIGA_OLD: u32 = 7;
const DENSITY_BRIERLEY_AMIGA: u32 = 8;
const DENSITY_BRIERLEY_AMIGA_KEY: u32 = 9;
const DENSITY_NOMINAL_PERMILLE: i32 = 1000;

// Data-stream sample types.
const DATA_SYNC: u8 = 1;
const DATA_DATA: u8 = 2;
const DATA_GAP: u8 = 3;
const DATA_RAW: u8 = 4;
const DATA_WEAK: u8 = 5;

// Gap-stream sample types.
const GAP_LENGTH: u8 = 1;
const GAP_DATA: u8 = 2;

// Block descriptor flags. Only an SPS-encoded file sets them.
const BLOCK_FLAG_FWGAP: u32 = 1 << 0;
const BLOCK_FLAG_BWGAP: u32 = 1 << 1;
const BLOCK_FLAG_BIT_LENGTHS: u32 = 1 << 2;

/// One decoded revolution: packed MFM words, MSB first, and the exact bit
/// length it wraps at. `density` is the track's mastered cell-rate profile as
/// `(start_bit, permille)` spans in ascending bit order, each holding until
/// the next one: cells from `start_bit` are written at `permille` / 1000 of
/// the nominal cell time. Empty for a track written at one rate throughout.
#[derive(Debug)]
pub struct IpfTrack {
    pub words: Vec<u16>,
    pub bit_len: u32,
    pub density: Vec<(u32, u16)>,
}

/// True when `data` opens with the IPF/CAPS file signature.
pub fn is_ipf(data: &[u8]) -> bool {
    data.starts_with(CAPS_SIGNATURE)
}

/// Decode a whole IPF file into per-track revolutions, indexed by
/// `cylinder * 2 + head`. Tracks the image leaves unformatted come back as
/// `None`, which is how [`crate::floppy`] already spells "no medium here".
pub fn decode(data: &[u8]) -> Result<Vec<Option<IpfTrack>>> {
    ensure!(is_ipf(data), "missing IPF/CAPS signature");

    let mut info: Option<Info> = None;
    let mut images: Vec<Imge> = Vec::new();
    let mut areas: Vec<(u32, &[u8])> = Vec::new();

    for chunk in Chunks::new(data) {
        let chunk = chunk?;
        match &chunk.name {
            b"CAPS" => {}
            b"INFO" => {
                ensure!(info.is_none(), "IPF image has more than one INFO chunk");
                info = Some(Info::parse(chunk.body)?);
            }
            b"IMGE" => images.push(Imge::parse(chunk.body)?),
            b"DATA" => {
                let (key, area) = parse_data(chunk.body, chunk.extra)?;
                areas.push((key, area));
            }
            // Unknown chunks are skipped: the container is self-describing and
            // a newer writer may add its own without invalidating the tracks.
            _ => {}
        }
    }

    let info = info.context("IPF image has no INFO chunk")?;
    ensure!(
        info.media_type == MEDIA_FLOPPY,
        "IPF media type {} is not a floppy disk",
        info.media_type
    );
    ensure!(!images.is_empty(), "IPF image describes no tracks");

    let mut tracks: Vec<Option<IpfTrack>> = (0..MAX_TRACKS).map(|_| None).collect();
    let mut unknown_density = None;
    for imge in &images {
        let index = imge.track_index()?;
        ensure!(
            tracks[index].is_none(),
            "IPF image describes cylinder {} head {} twice",
            imge.cylinder,
            imge.head
        );
        if let Some(density) = imge.unknown_density() {
            unknown_density.get_or_insert(density);
        }
        let area = areas
            .iter()
            .find(|(key, _)| *key == imge.data_key)
            .map(|(_, area)| *area);
        tracks[index] = decode_track(imge, area, &info).with_context(|| {
            format!("decoding IPF cylinder {} head {}", imge.cylinder, imge.head)
        })?;
    }

    if let Some(density) = unknown_density {
        // The cells are still decoded, and every byte of them is right; it is
        // only the varying cell *rate* that is missing, so the disk loads and
        // just the timing check of the protection sees the wrong answer.
        warn!(
            "IPF image uses cell-density model {density}, which Copperline does not know; the \
             track data is decoded with uniform 2 us cells, so a protection that measures \
             cell timing may not pass"
        );
    }

    ensure!(
        tracks.iter().any(|t| t.is_some()),
        "IPF image contains no formatted tracks"
    );
    Ok(tracks)
}

/// A chunk: name, the body covered by the length field, and -- for `DATA` --
/// the data area that trails it.
struct Chunk<'a> {
    name: [u8; 4],
    body: &'a [u8],
    extra: &'a [u8],
}

struct Chunks<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Chunks<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    fn next_chunk(&mut self) -> Result<Option<Chunk<'a>>> {
        if self.pos >= self.data.len() {
            return Ok(None);
        }
        ensure!(
            self.pos + CHUNK_HEADER_LEN <= self.data.len(),
            "IPF chunk header at offset {} is truncated",
            self.pos
        );
        let header = &self.data[self.pos..self.pos + CHUNK_HEADER_LEN];
        let name: [u8; 4] = header[0..4].try_into().expect("four-byte chunk name");
        let len = read_be_u32(&header[4..8]) as usize;
        let crc = read_be_u32(&header[8..12]);
        ensure!(
            len >= CHUNK_HEADER_LEN,
            "IPF chunk {} declares a {len}-byte length, shorter than its header",
            name_str(&name)
        );
        let end = self
            .pos
            .checked_add(len)
            .context("IPF chunk length overflow")?;
        ensure!(
            end <= self.data.len(),
            "IPF chunk {} at offset {} runs past the end of the file",
            name_str(&name),
            self.pos
        );

        // The stored CRC covers the chunk with its own CRC field zeroed.
        let mut sum = Crc::new();
        sum.update(&self.data[self.pos..self.pos + 8]);
        sum.update(&[0, 0, 0, 0]);
        sum.update(&self.data[self.pos + CHUNK_HEADER_LEN..end]);
        ensure!(
            sum.sum() == crc,
            "IPF chunk {} CRC mismatch: expected {crc:08X}, got {:08X}",
            name_str(&name),
            sum.sum()
        );

        let body = &self.data[self.pos + CHUNK_HEADER_LEN..end];
        // A DATA chunk's data area follows the chunk and is not counted in the
        // chunk length, so the walk has to step over it as well.
        let extra_len = if &name == b"DATA" {
            ensure!(
                body.len() >= 4,
                "IPF DATA chunk is too short to hold its data-area size"
            );
            read_be_u32(&body[0..4]) as usize
        } else {
            0
        };
        let extra_end = end
            .checked_add(extra_len)
            .context("IPF data area length overflow")?;
        ensure!(
            extra_end <= self.data.len(),
            "IPF data area at offset {end} runs past the end of the file"
        );
        let extra = &self.data[end..extra_end];

        self.pos = extra_end;
        Ok(Some(Chunk { name, body, extra }))
    }
}

impl<'a> Iterator for Chunks<'a> {
    type Item = Result<Chunk<'a>>;

    fn next(&mut self) -> Option<Self::Item> {
        match self.next_chunk() {
            Ok(Some(chunk)) => Some(Ok(chunk)),
            Ok(None) => None,
            Err(err) => {
                // Stop after reporting: the walk cannot resynchronise.
                self.pos = self.data.len();
                Some(Err(err))
            }
        }
    }
}

struct Info {
    media_type: u32,
    encoder_type: u32,
    encoder_rev: u32,
}

impl Info {
    fn parse(body: &[u8]) -> Result<Self> {
        ensure!(body.len() >= INFO_BODY_LEN, "IPF INFO chunk is truncated");
        Ok(Self {
            media_type: read_be_u32(&body[0..4]),
            encoder_type: read_be_u32(&body[4..8]),
            encoder_rev: read_be_u32(&body[8..12]),
        })
    }

    /// Whether a block descriptor's flags word can be believed. Reading gap
    /// streams out of it needs two separate fields to be defined, and each
    /// condition here guards one of them:
    ///
    /// - the encoder *type* fixes the descriptor's union: a CAPS-encoded file
    ///   spends the word an SPS one uses for the gap-stream offset on the
    ///   block's byte count instead, so there is no offset to follow even if
    ///   the flags asked for one;
    /// - the encoder *release* fixes the flags word itself, which release 1
    ///   leaves undefined and which is therefore read as zero.
    ///
    /// So the flags are only acted on when both hold. Erring this way costs at
    /// most the gap fill -- a gap whose streams are skipped is still laid down
    /// at its declared length from the descriptor's gap byte -- whereas
    /// trusting the word too readily would send the parser chasing a
    /// gap-stream offset that is really a byte count.
    fn block_flags_meaningful(&self) -> bool {
        self.encoder_type != ENCODER_CAPS && self.encoder_rev != 1
    }
}

struct Imge {
    cylinder: u32,
    head: u32,
    density: u32,
    signal_type: u32,
    start_bit_pos: u32,
    gap_bits: u32,
    track_bits: u32,
    block_count: u32,
    data_key: u32,
}

impl Imge {
    fn parse(body: &[u8]) -> Result<Self> {
        ensure!(body.len() >= IMGE_BODY_LEN, "IPF IMGE chunk is truncated");
        Ok(Self {
            cylinder: read_be_u32(&body[0..4]),
            head: read_be_u32(&body[4..8]),
            density: read_be_u32(&body[8..12]),
            signal_type: read_be_u32(&body[12..16]),
            start_bit_pos: read_be_u32(&body[24..28]),
            gap_bits: read_be_u32(&body[32..36]),
            track_bits: read_be_u32(&body[36..40]),
            block_count: read_be_u32(&body[40..44]),
            data_key: read_be_u32(&body[52..56]),
        })
    }

    fn track_index(&self) -> Result<usize> {
        ensure!(self.head < 2, "IPF track head {} is not 0 or 1", self.head);
        let index = (self.cylinder as usize)
            .checked_mul(2)
            .and_then(|c| c.checked_add(self.head as usize))
            .context("IPF track index overflow")?;
        ensure!(
            index < MAX_TRACKS,
            "IPF cylinder {} head {} is outside the {MAX_TRACKS}-track range Copperline models",
            self.cylinder,
            self.head
        );
        Ok(index)
    }

    /// A density model this decoder has no cell-rate profile for.
    fn unknown_density(&self) -> Option<u32> {
        match self.density {
            DENSITY_NOISE | DENSITY_AUTO => None,
            DENSITY_COPYLOCK_AMIGA..=DENSITY_BRIERLEY_AMIGA_KEY => None,
            other => Some(other),
        }
    }
}

/// A `DATA` chunk: the data key it answers to, and its data area.
fn parse_data<'a>(body: &'a [u8], extra: &'a [u8]) -> Result<(u32, &'a [u8])> {
    ensure!(body.len() >= DATA_BODY_LEN, "IPF DATA chunk is truncated");
    let size = read_be_u32(&body[0..4]) as usize;
    let bit_size = read_be_u32(&body[4..8]) as usize;
    let crc = read_be_u32(&body[8..12]);
    let key = read_be_u32(&body[12..16]);
    ensure!(
        extra.len() == size,
        "IPF data area for key {key} is {} bytes, not the {size} the chunk declares",
        extra.len()
    );
    ensure!(
        bit_size == size * 8,
        "IPF data area for key {key} declares {bit_size} bits for {size} bytes"
    );
    let mut sum = Crc::new();
    sum.update(extra);
    ensure!(
        sum.sum() == crc,
        "IPF data area for key {key} CRC mismatch: expected {crc:08X}, got {:08X}",
        sum.sum()
    );
    Ok((key, extra))
}

/// One block descriptor out of the head of a data area.
struct Block {
    block_bits: usize,
    gap_bits: usize,
    /// SPS only: where this block's gap streams sit in the data area.
    gap_offset: usize,
    flags: u32,
    gap_value: u8,
    /// The whole gap-value word: block 0's doubles as the density key of the
    /// Brierley density-key model.
    gap_value_word: u32,
    data_offset: usize,
}

impl Block {
    fn parse(area: &[u8], index: usize, flags_meaningful: bool) -> Result<Self> {
        let start = index * BLOCK_DESCRIPTOR_LEN;
        let end = start + BLOCK_DESCRIPTOR_LEN;
        ensure!(
            end <= area.len(),
            "IPF block descriptor {index} runs past the data area"
        );
        let d = &area[start..end];
        Ok(Self {
            block_bits: read_be_u32(&d[0..4]) as usize,
            gap_bits: read_be_u32(&d[4..8]) as usize,
            gap_offset: read_be_u32(&d[8..12]) as usize,
            flags: if flags_meaningful {
                read_be_u32(&d[20..24])
            } else {
                0
            },
            // The gap fill value is a byte held in a big-endian word.
            gap_value: d[27],
            gap_value_word: read_be_u32(&d[24..28]),
            data_offset: read_be_u32(&d[28..32]) as usize,
        })
    }

    fn sample_lengths_in_bits(&self) -> bool {
        self.flags & BLOCK_FLAG_BIT_LENGTHS != 0
    }
}

fn decode_track(imge: &Imge, area: Option<&[u8]>, info: &Info) -> Result<Option<IpfTrack>> {
    // An unformatted track carries no blocks and no cells: the head passes
    // over noise, which is the same nothing a drive with no image reads.
    if imge.block_count == 0 || imge.track_bits == 0 {
        return Ok(None);
    }
    ensure!(
        imge.signal_type == SIGNAL_2US_CELLS,
        "IPF signal type {} is not the 2 us cell rate of an Amiga DD disk",
        imge.signal_type
    );
    let track_bits = imge.track_bits as usize;
    ensure!(
        track_bits <= MAX_TRACK_BITS,
        "IPF track length {track_bits} bits exceeds the {MAX_TRACK_BITS}-bit limit"
    );
    let area = area.with_context(|| format!("IPF data key {} has no DATA chunk", imge.data_key))?;

    // MFM sets a cell's clock bit only when the data bits either side of it
    // are zero, so the very first clock of the revolution depends on the last
    // data bit of the revolution before it -- the same one, since the track
    // loops. Encode once to learn that bit, then again with it in hand.
    let seed = encode_blocks(imge, area, info, false)?.1;
    let (bits, _) = encode_blocks(imge, area, info, seed)?;
    ensure!(
        bits.len() == track_bits,
        "IPF track holds {} bits, not the {track_bits} its descriptor declares",
        bits.len()
    );

    // The blocks were laid out from the first one; the descriptor says how far
    // past the index that block begins, so rotating by it puts the revolution
    // back the way the head met it.
    let start = imge.start_bit_pos as usize % track_bits;
    let mut words = vec![0u16; track_bits.div_ceil(16)];
    for (i, bit) in bits.iter().enumerate() {
        if *bit {
            let pos = (start + i) % track_bits;
            words[pos / 16] |= 0x8000 >> (pos % 16);
        }
    }

    let flags_meaningful = info.block_flags_meaningful();
    let blocks = (0..imge.block_count as usize)
        .map(|index| Block::parse(area, index, flags_meaningful))
        .collect::<Result<Vec<_>>>()?;
    let density = density_profile(imge, &blocks, start, track_bits);

    Ok(Some(IpfTrack {
        words,
        bit_len: imge.track_bits,
        density,
    }))
}

/// The track's mastered cell-rate profile as `(start_bit, permille)` spans on
/// the revolution as rotated to the index, or empty when every cell is
/// nominal. The profiles are the CAPS library's own (`GenerateCLA`,
/// `GenerateSLA`, `GenerateABA` and their variants), which weight each block's
/// cell time in per-mille of nominal:
///
/// - Copylock Amiga slows and speeds three consecutive sectors (blocks 4-6 on
///   the original scheme, 0-2 on the newer one) by -5.5%, -0.5% and +4.5%,
///   each run starting at the gap that precedes its block. A loader reads
///   two of them byte by byte, counting how often it polled between bytes,
///   and passes only if their counts differ by the few per cent the mastered
///   rates put between them;
/// - Copylock ST writes block 5 5% slow; Speedlock writes block 1 10% slow
///   and block 2 10% fast (the older variant block 1 5% slow); Brierley steps
///   blocks 1, 2, 4, 5 and 6 through +10%, +5%, -5%, -10%, -15%; and the
///   Brierley density-key model takes a bit per block from block 0's gap
///   value word, 5% fast where set and 5% slow where clear.
fn density_profile(
    imge: &Imge,
    blocks: &[Block],
    start: usize,
    track_bits: usize,
) -> Vec<(u32, u16)> {
    // Per affected block: the per-mille delta, and whether the run begins at
    // the gap that precedes the block rather than at the block itself.
    let deltas: Vec<(usize, i32, bool)> = match imge.density {
        DENSITY_COPYLOCK_AMIGA => vec![(4, -55, true), (5, -5, true), (6, 45, true)],
        DENSITY_COPYLOCK_AMIGA_NEW => vec![(0, -55, true), (1, -5, true), (2, 45, true)],
        DENSITY_COPYLOCK_ST => vec![(5, 50, false)],
        DENSITY_SPEEDLOCK_AMIGA => vec![(1, 100, false), (2, -100, false)],
        DENSITY_SPEEDLOCK_AMIGA_OLD => vec![(1, 50, false)],
        DENSITY_BRIERLEY_AMIGA => vec![
            (1, 100, false),
            (2, 50, false),
            (4, -50, false),
            (5, -100, false),
            (6, -150, false),
        ],
        DENSITY_BRIERLEY_AMIGA_KEY => {
            let key = blocks.first().map_or(0, |block| block.gap_value_word);
            (1..blocks.len())
                .map(|blk| {
                    let mask = 1u32.checked_shl(blk as u32 - 1).unwrap_or(0);
                    (blk, if key & mask != 0 { -50 } else { 50 }, false)
                })
                .collect()
        }
        _ => return Vec::new(),
    };
    if track_bits == 0 || blocks.is_empty() {
        return Vec::new();
    }

    // Where each block begins in the stream as laid out from block 0; its
    // gap follows it.
    let mut block_starts = Vec::with_capacity(blocks.len());
    let mut pos = 0usize;
    for block in blocks {
        block_starts.push(pos);
        pos += block.block_bits + block.gap_bits;
    }
    let mut weights = vec![DENSITY_NOMINAL_PERMILLE; track_bits];
    for (blk, delta, from_preceding_gap) in deltas {
        let Some(&block_start) = block_starts.get(blk) else {
            continue;
        };
        let lead = if from_preceding_gap {
            blocks[(blk + blocks.len() - 1) % blocks.len()].gap_bits
        } else {
            0
        };
        let from = block_start as isize - lead as isize;
        let to = (block_start + blocks[blk].block_bits) as isize;
        for i in from..to {
            weights[i.rem_euclid(track_bits as isize) as usize] += delta;
        }
    }

    // Rotate to the index as the cells were, then keep only the changes.
    let mut spans: Vec<(u32, u16)> = Vec::new();
    for bit in 0..track_bits {
        let weight = weights[(bit + track_bits - start) % track_bits];
        let permille = weight.clamp(1, i32::from(u16::MAX)) as u16;
        if spans.last().is_none_or(|&(_, last)| last != permille) {
            spans.push((bit as u32, permille));
        }
    }
    if spans
        .iter()
        .all(|&(_, permille)| i32::from(permille) == DENSITY_NOMINAL_PERMILLE)
    {
        return Vec::new();
    }
    spans
}

/// Encode every block of a track back to back, returning the cells and the
/// last decoded data bit written (the seed the next revolution starts from).
fn encode_blocks(imge: &Imge, area: &[u8], info: &Info, seed: bool) -> Result<(Vec<bool>, bool)> {
    let flags_meaningful = info.block_flags_meaningful();
    let mut out: Vec<bool> = Vec::with_capacity(imge.track_bits as usize);
    let mut prev = seed;
    let mut total_gap = 0usize;

    for index in 0..imge.block_count as usize {
        let block = Block::parse(area, index, flags_meaningful)?;
        let start = out.len();
        if block.block_bits > 0 {
            decode_data_stream(area, &block, &mut out, &mut prev)
                .with_context(|| format!("in block {index} data stream"))?;
            let written = out.len() - start;
            ensure!(
                written == block.block_bits,
                "IPF block {index} produced {written} bits, not the {} it declares",
                block.block_bits
            );
        }
        if block.gap_bits > 0 {
            fill_gap(area, &block, &mut out, &mut prev)
                .with_context(|| format!("in block {index} gap"))?;
            total_gap += block.gap_bits;
        }
    }

    ensure!(
        total_gap == imge.gap_bits as usize,
        "IPF track gap totals {total_gap} bits, not the {} the track declares",
        imge.gap_bits
    );
    Ok((out, prev))
}

/// Walk a data stream, appending the cells each sample stands for.
fn decode_data_stream(
    area: &[u8],
    block: &Block,
    out: &mut Vec<bool>,
    prev: &mut bool,
) -> Result<()> {
    let mut pos = block.data_offset;
    loop {
        let Some((kind, size, next)) = read_sample_header(area, pos)? else {
            return Ok(());
        };
        let in_bits = block.sample_lengths_in_bits();
        let bits = if in_bits { size } else { size * 8 };
        // A bit-counted sample is padded out so the next one starts on a byte
        // boundary.
        let bytes = bits.div_ceil(8);
        let end = next
            .checked_add(bytes)
            .context("IPF data sample length overflow")?;
        ensure!(
            end <= area.len(),
            "IPF data sample runs past the end of the data area"
        );
        let sample = &area[next..end];

        match kind {
            // Already-encoded cells: written through exactly as stored, which
            // is what keeps a sync mark's illegal clocking intact.
            DATA_SYNC | DATA_RAW => push_raw(out, prev, sample, bits),
            // Weak cells read back differently every revolution on real media.
            // Copperline reads a deterministic revolution, so the stored
            // sample is written through as it stands.
            // TODO: carry the weak spans through to FloppyTrackImage::RawMfm
            // as extra revolutions with the fuzzy runs varied per revolution.
            DATA_WEAK => push_raw(out, prev, sample, bits),
            // Decoded payload: one cell per data bit, the clock bit set only
            // where it separates two zero data bits.
            DATA_DATA | DATA_GAP => {
                for i in 0..bits {
                    let data = sample[i / 8] & (0x80 >> (i % 8)) != 0;
                    out.push(!*prev && !data);
                    out.push(data);
                    *prev = data;
                }
            }
            other => bail!("unknown IPF data sample type {other}"),
        }
        pos = end;
    }
}

/// Fill a block's gap, from a repeated byte or from its gap streams.
fn fill_gap(area: &[u8], block: &Block, out: &mut Vec<bool>, prev: &mut bool) -> Result<()> {
    let budget = block.gap_bits;
    let has_forward = block.flags & BLOCK_FLAG_FWGAP != 0;
    let has_backward = block.flags & BLOCK_FLAG_BWGAP != 0;

    if !has_forward && !has_backward {
        // No streams: repeat the descriptor's gap byte. The writer laid the
        // gap down from both ends at once, so the splice falls in the middle;
        // the trailing half is aligned to the gap's end rather than its start.
        let sample = byte_bits(block.gap_value);
        let half = budget / 2;
        fill_mfm(out, prev, &sample, half);
        fill_mfm_end_aligned(out, prev, &sample, budget - half);
        return Ok(());
    }

    let mut pos = block.gap_offset;
    // Offset zero lands inside the block descriptors, so it means the block
    // claimed gap streams without saying where they are.
    ensure!(
        pos > 0,
        "IPF block declares gap streams but gives no gap-stream offset"
    );
    ensure!(
        pos < area.len(),
        "IPF gap-stream offset {pos} is outside the {}-byte data area",
        area.len()
    );
    let forward = if has_forward {
        let (stream, next) = GapStream::parse(area, pos)?;
        pos = next;
        Some(stream)
    } else {
        None
    };
    let backward = if has_backward {
        let (stream, _) = GapStream::parse(area, pos)?;
        Some(stream)
    } else {
        None
    };

    let fwd_fixed = forward.as_ref().map_or(0, GapStream::fixed_bits);
    let bwd_fixed = backward.as_ref().map_or(0, GapStream::fixed_bits);

    // Share the gap out between the fixed parts. Where they overflow it they
    // are evenly truncated, and whatever one stream leaves unused goes to the
    // other.
    let half = budget / 2;
    let mut fwd = fwd_fixed.min(half);
    let mut bwd = bwd_fixed.min(budget - half);
    fwd += (fwd_fixed - fwd).min(budget - fwd - bwd);
    bwd += (bwd_fixed - bwd).min(budget - fwd - bwd);

    // Whatever is left over is taken up by the loop samples: both streams
    // stretch to meet in the middle, or the only one that can stretch fills it.
    let slack = budget - fwd - bwd;
    let fwd_loops = forward.as_ref().is_some_and(GapStream::can_loop);
    let bwd_loops = backward.as_ref().is_some_and(GapStream::can_loop);
    let (fwd_slack, bwd_slack) = match (fwd_loops, bwd_loops) {
        (true, true) => (slack / 2, slack - slack / 2),
        (true, false) => (slack, 0),
        (false, true) => (0, slack),
        // Nothing can stretch, so the gap byte makes up the difference.
        (false, false) => (0, 0),
    };
    let unfilled = slack - fwd_slack - bwd_slack;

    if let Some(stream) = &forward {
        stream.emit_fixed(out, prev, fwd, false);
        stream.emit_loop(out, prev, fwd_slack, false);
    }
    if unfilled > 0 {
        fill_mfm(out, prev, &byte_bits(block.gap_value), unfilled);
    }
    if let Some(stream) = &backward {
        stream.emit_loop(out, prev, bwd_slack, true);
        stream.emit_fixed(out, prev, bwd, true);
    }
    Ok(())
}

/// A gap stream: its fixed-length samples, plus the sample that stretches to
/// take up the slack, if it has one.
struct GapStream {
    /// Decoded-bit length and the decoded bits to repeat, per sample.
    fixed: Vec<(usize, Vec<bool>)>,
    loop_sample: Option<Vec<bool>>,
    /// An explicit zero-length loop sample forbids stretching outright.
    loop_disabled: bool,
}

impl GapStream {
    /// Samples pair an optional length with the data to repeat; the one that
    /// arrives without a length is the loop sample. Lengths here are always in
    /// bits, whatever the block's flags say about the data stream.
    fn parse(area: &[u8], mut pos: usize) -> Result<(Self, usize)> {
        let mut fixed = Vec::new();
        let mut loop_sample = None;
        let mut loop_disabled = false;
        let mut pending: Option<usize> = None;
        loop {
            let Some((kind, size, next)) = read_sample_header(area, pos)? else {
                return Ok((
                    Self {
                        fixed,
                        loop_sample,
                        loop_disabled,
                    },
                    pos + 1,
                ));
            };
            match kind {
                GAP_LENGTH => {
                    pos = next;
                    pending = Some(size);
                }
                GAP_DATA => {
                    let bytes = size.div_ceil(8);
                    let end = next
                        .checked_add(bytes)
                        .context("IPF gap sample length overflow")?;
                    ensure!(
                        end <= area.len(),
                        "IPF gap sample runs past the end of the data area"
                    );
                    let sample = bits_of(&area[next..end], size);
                    match pending.take() {
                        Some(length) => fixed.push((length, sample)),
                        None if size == 0 => loop_disabled = true,
                        None => loop_sample = Some(sample),
                    }
                    pos = end;
                }
                other => bail!("unknown IPF gap sample type {other}"),
            }
        }
    }

    /// How many cells the fixed samples lay down: each decoded bit is one MFM
    /// cell, and a cell is two bits.
    fn fixed_bits(&self) -> usize {
        self.fixed.iter().map(|(len, _)| len * 2).sum()
    }

    /// A stream stretches from its explicit loop sample or, failing that, by
    /// repeating the last sample it holds.
    fn can_loop(&self) -> bool {
        !self.loop_disabled && (self.loop_sample.is_some() || !self.fixed.is_empty())
    }

    fn emit_fixed(&self, out: &mut Vec<bool>, prev: &mut bool, budget: usize, from_end: bool) {
        if budget == 0 {
            return;
        }
        let mut bits = Vec::with_capacity(budget);
        let mut scratch = *prev;
        for (len, sample) in &self.fixed {
            fill_mfm(&mut bits, &mut scratch, sample, len * 2);
        }
        // A backward stream is laid down towards the gap's end, so it is the
        // tail of it that survives truncation.
        emit_slice(out, prev, &bits, budget, from_end);
    }

    fn emit_loop(&self, out: &mut Vec<bool>, prev: &mut bool, budget: usize, from_end: bool) {
        if budget == 0 {
            return;
        }
        // Without an explicit loop sample the decoder repeats the last sample
        // it came across -- the first one, for a stream applied in reverse.
        let sample = self.loop_sample.clone().unwrap_or_else(|| {
            let pick = if from_end {
                self.fixed.first()
            } else {
                self.fixed.last()
            };
            pick.map(|(_, s)| s.clone()).unwrap_or_default()
        });
        if from_end {
            fill_mfm_end_aligned(out, prev, &sample, budget);
        } else {
            fill_mfm(out, prev, &sample, budget);
        }
    }
}

/// Append `budget` cells from `bits`, taking them from its end when the
/// content is aligned to the end of the run rather than its start.
fn emit_slice(out: &mut Vec<bool>, prev: &mut bool, bits: &[bool], budget: usize, from_end: bool) {
    let slice = if bits.len() <= budget {
        bits
    } else if from_end {
        &bits[bits.len() - budget..]
    } else {
        &bits[..budget]
    };
    out.extend_from_slice(slice);
    // A cell is a clock bit then a data bit, so the data bits are the odd
    // indices and the last one is the end of the last whole cell.
    if let Some(last) = slice
        .len()
        .checked_sub(if slice.len() % 2 == 0 { 1 } else { 2 })
    {
        *prev = slice[last];
    }
    // A short run is padded to the budget so the gap still comes out the
    // declared length.
    for _ in slice.len()..budget {
        out.push(false);
    }
}

/// MFM-encode `sample`, repeating it until `budget` cells have been written.
fn fill_mfm(out: &mut Vec<bool>, prev: &mut bool, sample: &[bool], budget: usize) {
    if budget == 0 {
        return;
    }
    if sample.is_empty() {
        for _ in 0..budget {
            out.push(false);
        }
        return;
    }
    let target = out.len() + budget;
    let mut i = 0;
    while out.len() < target {
        let data = sample[i % sample.len()];
        out.push(!*prev && !data);
        if out.len() == target {
            // The gap ended between a cell's clock and its data bit.
            break;
        }
        out.push(data);
        *prev = data;
        i += 1;
    }
}

/// As [`fill_mfm`], but with the repeat aligned so the sample ends flush with
/// the end of the run -- how a gap written backwards from the next block sits.
fn fill_mfm_end_aligned(out: &mut Vec<bool>, prev: &mut bool, sample: &[bool], budget: usize) {
    if budget == 0 {
        return;
    }
    if sample.is_empty() {
        for _ in 0..budget {
            out.push(false);
        }
        return;
    }
    // Generate a whole number of repeats past the budget, then keep the tail.
    let period = sample.len() * 2;
    let generated = budget.div_ceil(period) * period + period;
    let mut bits = Vec::with_capacity(generated);
    let mut scratch = *prev;
    fill_mfm(&mut bits, &mut scratch, sample, generated);
    emit_slice(out, prev, &bits, budget, true);
}

/// Read a sample header: its type and length, and where its data starts.
/// `None` at the nul byte that ends a stream.
fn read_sample_header(area: &[u8], pos: usize) -> Result<Option<(u8, usize, usize)>> {
    ensure!(
        pos < area.len(),
        "IPF stream runs past the end of the data area"
    );
    let header = area[pos];
    if header == 0 {
        return Ok(None);
    }
    // The top three bits count the length bytes that follow; the low five are
    // the sample type.
    let width = (header >> 5) as usize;
    let kind = header & 0x1f;
    let start = pos + 1;
    let end = start
        .checked_add(width)
        .context("IPF sample header overflow")?;
    ensure!(
        end <= area.len(),
        "IPF sample length field runs past the end of the data area"
    );
    let mut size = 0usize;
    for byte in &area[start..end] {
        size = size
            .checked_mul(256)
            .and_then(|s| s.checked_add(*byte as usize))
            .context("IPF sample length overflow")?;
    }
    Ok(Some((kind, size, end)))
}

/// Write already-encoded cells through unchanged.
fn push_raw(out: &mut Vec<bool>, prev: &mut bool, sample: &[u8], bits: usize) {
    for i in 0..bits {
        let bit = sample[i / 8] & (0x80 >> (i % 8)) != 0;
        out.push(bit);
        // A cell is a clock bit then a data bit, so every odd index is a data
        // bit and the last one to land is what the next encoded sample follows
        // on from. A bit-counted sample may stop part way through a cell, which
        // simply leaves the preceding data bit standing.
        if i % 2 == 1 {
            *prev = bit;
        }
    }
}

fn byte_bits(value: u8) -> Vec<bool> {
    (0..8).rev().map(|i| value & (1 << i) != 0).collect()
}

fn bits_of(bytes: &[u8], count: usize) -> Vec<bool> {
    (0..count)
        .map(|i| bytes[i / 8] & (0x80 >> (i % 8)) != 0)
        .collect()
}

fn read_be_u32(bytes: &[u8]) -> u32 {
    u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
}

fn name_str(name: &[u8; 4]) -> String {
    String::from_utf8_lossy(name).to_string()
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// The revolution an AmigaDOS track occupies in the fixture below, and in
    /// the SPS releases it is modelled on: 11 sectors and the index gap.
    pub(crate) const AMIGADOS_TRACK_BITS: u32 = 100_128;
    const SECTOR_BYTES: usize = 540;
    /// Two gap bytes and a doubled sync mark ahead of the sector payload.
    const BLOCK_BITS: usize = 2 * 16 + 4 * 8 + SECTOR_BYTES * 16;
    const SECTORS: usize = 11;
    const GAP_BITS: usize = AMIGADOS_TRACK_BITS as usize - SECTORS * BLOCK_BITS;

    // -- fixture construction ------------------------------------------------

    /// Wrap a body as a chunk, filling in the CRC the decoder checks.
    fn chunk(name: &[u8; 4], body: &[u8], extra: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(CHUNK_HEADER_LEN + body.len() + extra.len());
        out.extend_from_slice(name);
        out.extend_from_slice(&((CHUNK_HEADER_LEN + body.len()) as u32).to_be_bytes());
        out.extend_from_slice(&[0; 4]);
        out.extend_from_slice(body);
        let mut crc = Crc::new();
        crc.update(&out);
        let sum = crc.sum().to_be_bytes();
        out[8..12].copy_from_slice(&sum);
        out.extend_from_slice(extra);
        out
    }

    fn info_body(encoder: u32, encoder_rev: u32) -> Vec<u8> {
        let mut body = vec![0u8; INFO_BODY_LEN];
        body[0..4].copy_from_slice(&MEDIA_FLOPPY.to_be_bytes());
        body[4..8].copy_from_slice(&encoder.to_be_bytes());
        body[8..12].copy_from_slice(&encoder_rev.to_be_bytes());
        body
    }

    #[derive(Clone, Copy)]
    struct Track {
        density: u32,
        signal: u32,
        start_bit: u32,
        data_bits: u32,
        gap_bits: u32,
        track_bits: u32,
        blocks: u32,
    }

    impl Default for Track {
        fn default() -> Self {
            Self {
                density: DENSITY_AUTO,
                signal: SIGNAL_2US_CELLS,
                start_bit: 0,
                data_bits: 0,
                gap_bits: 0,
                track_bits: 0,
                blocks: 1,
            }
        }
    }

    fn imge_body(track: &Track, key: u32) -> Vec<u8> {
        let mut body = vec![0u8; IMGE_BODY_LEN];
        let put = |body: &mut Vec<u8>, at: usize, value: u32| {
            body[at..at + 4].copy_from_slice(&value.to_be_bytes());
        };
        put(&mut body, 8, track.density);
        put(&mut body, 12, track.signal);
        put(&mut body, 16, track.track_bits / 8);
        put(&mut body, 20, track.start_bit / 8);
        put(&mut body, 24, track.start_bit);
        put(&mut body, 28, track.data_bits);
        put(&mut body, 32, track.gap_bits);
        put(&mut body, 36, track.track_bits);
        put(&mut body, 40, track.blocks);
        put(&mut body, 52, key);
        body
    }

    fn data_chunk(key: u32, area: &[u8]) -> Vec<u8> {
        let mut body = Vec::with_capacity(DATA_BODY_LEN);
        body.extend_from_slice(&(area.len() as u32).to_be_bytes());
        body.extend_from_slice(&((area.len() * 8) as u32).to_be_bytes());
        let mut crc = Crc::new();
        crc.update(area);
        body.extend_from_slice(&crc.sum().to_be_bytes());
        body.extend_from_slice(&key.to_be_bytes());
        chunk(b"DATA", &body, area)
    }

    fn block_descriptor(
        block_bits: usize,
        gap_bits: usize,
        gap_offset: usize,
        flags: u32,
        gap_value: u8,
        data_offset: usize,
    ) -> Vec<u8> {
        let mut d = vec![0u8; BLOCK_DESCRIPTOR_LEN];
        d[0..4].copy_from_slice(&(block_bits as u32).to_be_bytes());
        d[4..8].copy_from_slice(&(gap_bits as u32).to_be_bytes());
        d[8..12].copy_from_slice(&(gap_offset as u32).to_be_bytes());
        d[20..24].copy_from_slice(&flags.to_be_bytes());
        d[27] = gap_value;
        d[28..32].copy_from_slice(&(data_offset as u32).to_be_bytes());
        d
    }

    /// A stream sample: the type byte carries the width of the length field in
    /// its top three bits.
    fn sample(kind: u8, size: usize, data: &[u8]) -> Vec<u8> {
        let width = if size < 0x100 {
            1
        } else if size < 0x1_0000 {
            2
        } else {
            4
        };
        let mut out = vec![(width as u8) << 5 | kind];
        out.extend_from_slice(&size.to_be_bytes()[size_of::<usize>() - width..]);
        out.extend_from_slice(data);
        out
    }

    /// Assemble a one-track image around a ready-made data area.
    fn image(track: Track, area: &[u8], encoder: u32, encoder_rev: u32) -> Vec<u8> {
        let mut out = chunk(CAPS_SIGNATURE, &[], &[]);
        out.extend_from_slice(&chunk(b"INFO", &info_body(encoder, encoder_rev), &[]));
        out.extend_from_slice(&chunk(b"IMGE", &imge_body(&track, 1), &[]));
        out.extend_from_slice(&data_chunk(1, area));
        out
    }

    /// A track shaped like a real AmigaDOS one: eleven sectors, each opening
    /// with gap bytes and a doubled `4489` sync mark, and an index gap.
    pub(crate) fn amigados_ipf_image() -> Vec<u8> {
        let payload: Vec<u8> = (0..SECTOR_BYTES).map(|i| (i % 251) as u8).collect();
        let mut stream = sample(DATA_GAP, 2, &[0, 0]);
        stream.extend_from_slice(&sample(DATA_SYNC, 4, &[0x44, 0x89, 0x44, 0x89]));
        stream.extend_from_slice(&sample(DATA_DATA, SECTOR_BYTES, &payload));
        stream.push(0);

        let descriptors = SECTORS * BLOCK_DESCRIPTOR_LEN;
        let mut area = Vec::new();
        for sector in 0..SECTORS {
            area.extend_from_slice(&block_descriptor(
                BLOCK_BITS,
                if sector == SECTORS - 1 { GAP_BITS } else { 0 },
                0,
                0,
                0,
                descriptors + sector * stream.len(),
            ));
        }
        for _ in 0..SECTORS {
            area.extend_from_slice(&stream);
        }

        let track = Track {
            // The index falls inside the gap, as it does on a written disk.
            start_bit: 2372,
            data_bits: (SECTORS * BLOCK_BITS) as u32,
            gap_bits: GAP_BITS as u32,
            track_bits: AMIGADOS_TRACK_BITS,
            blocks: SECTORS as u32,
            ..Track::default()
        };
        image(track, &area, ENCODER_CAPS, 1)
    }

    // -- helpers over a decoded revolution -----------------------------------

    fn bits(track: &IpfTrack) -> Vec<bool> {
        (0..track.bit_len as usize)
            .map(|i| track.words[i / 16] & (0x8000 >> (i % 16)) != 0)
            .collect()
    }

    fn only_track(image: &[u8]) -> IpfTrack {
        let mut tracks = decode(image).expect("the fixture should decode");
        tracks[0]
            .take()
            .expect("cylinder 0 head 0 should be present")
    }

    /// Strip the clock bits: every second cell bit is the data one.
    fn data_bits(cells: &[bool]) -> Vec<bool> {
        cells.iter().skip(1).step_by(2).copied().collect()
    }

    /// MFM never writes two flux transitions in a row. Anything encoded by
    /// this decoder must hold to that -- only a stored sync mark may not.
    fn assert_mfm_legal(cells: &[bool]) {
        for pair in cells.windows(2) {
            assert!(
                !(pair[0] && pair[1]),
                "adjacent set cells are not legal MFM"
            );
        }
    }

    /// `count` data bits of `byte` repeated, entered `phase` bits in.
    fn repeating_data_bits(byte: u8, phase: usize, count: usize) -> Vec<bool> {
        (0..count)
            .map(|i| byte & (0x80 >> ((i + phase) % 8)) != 0)
            .collect()
    }

    // -- tests ---------------------------------------------------------------

    #[test]
    fn amigados_track_decodes_to_its_declared_revolution() {
        let track = only_track(&amigados_ipf_image());
        assert_eq!(track.bit_len, AMIGADOS_TRACK_BITS);
        assert_eq!(
            track.words.len(),
            (AMIGADOS_TRACK_BITS as usize).div_ceil(16)
        );
    }

    /// A sync mark is stored already encoded precisely because its clocking is
    /// illegal MFM; passing it through an encoder would destroy it.
    #[test]
    fn sync_samples_are_written_through_unencoded() {
        let track = only_track(&amigados_ipf_image());
        let cells = bits(&track);
        let marks = (0..cells.len() - 32)
            .filter(|&i| {
                (0..16).all(|b| cells[i + b] == (0x4489 & (0x8000 >> b) != 0))
                    && (0..16).all(|b| cells[i + 16 + b] == (0x4489 & (0x8000 >> b) != 0))
            })
            .count();
        assert_eq!(marks, SECTORS, "every sector should keep its doubled sync");
    }

    /// The first block starts where the descriptor says it does relative to
    /// the index, which is what puts the index inside the gap.
    #[test]
    fn the_revolution_is_rotated_to_start_at_the_index() {
        let track = only_track(&amigados_ipf_image());
        let cells = bits(&track);
        // Block 0 opens with two MFM-encoded nul bytes, then the sync mark.
        let sync_at = 2372 + 32;
        assert!((0..16).all(|b| cells[sync_at + b] == (0x4489 & (0x8000 >> b) != 0)));
    }

    #[test]
    fn encoded_payload_sets_the_clock_bit_only_between_zero_data_bits() {
        // One block: four decoded bytes, no gap.
        let payload = [0x00u8, 0xff, 0x0f, 0xa5];
        let mut stream = sample(DATA_DATA, payload.len(), &payload);
        stream.push(0);
        let mut area = block_descriptor(payload.len() * 16, 0, 0, 0, 0, BLOCK_DESCRIPTOR_LEN);
        area.extend_from_slice(&stream);
        let track = Track {
            data_bits: (payload.len() * 16) as u32,
            track_bits: (payload.len() * 16) as u32,
            ..Track::default()
        };
        let decoded = only_track(&image(track, &area, ENCODER_CAPS, 1));
        let cells = bits(&decoded);
        assert_mfm_legal(&cells);

        // The data bits come back out exactly as they went in.
        let recovered = data_bits(&cells);
        let expected: Vec<bool> = payload
            .iter()
            .flat_map(|b| (0..8).map(move |i| b & (0x80 >> i) != 0))
            .collect();
        assert_eq!(recovered, expected);
    }

    #[test]
    fn raw_samples_bypass_the_encoder() {
        let raw = [0xaau8, 0x55];
        let mut stream = sample(DATA_RAW, raw.len(), &raw);
        stream.push(0);
        let mut area = block_descriptor(raw.len() * 8, 0, 0, 0, 0, BLOCK_DESCRIPTOR_LEN);
        area.extend_from_slice(&stream);
        let track = Track {
            data_bits: (raw.len() * 8) as u32,
            track_bits: (raw.len() * 8) as u32,
            ..Track::default()
        };
        let decoded = only_track(&image(track, &area, ENCODER_CAPS, 1));
        assert_eq!(decoded.words[0], 0xaa55);
    }

    /// A gap with no streams of its own repeats the descriptor's gap byte.
    #[test]
    fn a_gap_without_streams_repeats_its_gap_byte() {
        let gap_bits = 320;
        let area = block_descriptor(0, gap_bits, 0, 0, 0x4e, BLOCK_DESCRIPTOR_LEN);
        let track = Track {
            gap_bits: gap_bits as u32,
            track_bits: gap_bits as u32,
            ..Track::default()
        };
        let decoded = only_track(&image(track, &area, ENCODER_CAPS, 1));
        let cells = bits(&decoded);
        assert_eq!(cells.len(), gap_bits);
        assert_mfm_legal(&cells);
        assert_eq!(
            data_bits(&cells),
            repeating_data_bits(0x4e, 0, gap_bits / 2)
        );
    }

    /// The worked example from the format notes: a forward and a backward gap
    /// stream whose fixed parts fill the gap exactly, with no looping.
    #[test]
    fn paired_gap_streams_fill_the_gap_from_both_ends() {
        let gap_bits = 832;
        let streams: &[u8] = &[
            0x41, 0x01, 0x40, 0x22, 0x08, 0x4e, 0x00, // forward: 0x140 bits of 0x4e
            0x21, 0x60, 0x22, 0x08, 0x00, 0x00, // backward: 0x60 bits of 0x00
        ];
        let mut area = block_descriptor(
            0,
            gap_bits,
            BLOCK_DESCRIPTOR_LEN,
            BLOCK_FLAG_FWGAP | BLOCK_FLAG_BWGAP,
            0,
            0,
        );
        area.extend_from_slice(streams);
        let track = Track {
            gap_bits: gap_bits as u32,
            track_bits: gap_bits as u32,
            ..Track::default()
        };
        let decoded = only_track(&image(track, &area, ENCODER_SPS_TEST, 2));
        let cells = bits(&decoded);
        assert_eq!(cells.len(), gap_bits);
        assert_mfm_legal(&cells);

        // 0x140 decoded bits of 0x4e forwards, then 0x60 of nul backwards.
        let recovered = data_bits(&cells);
        assert_eq!(recovered[..0x140], repeating_data_bits(0x4e, 0, 0x140)[..]);
        assert!(recovered[0x140..].iter().all(|bit| !bit));
        assert_eq!(recovered.len(), 0x140 + 0x60);
    }

    /// The second worked example: loop samples stretch to meet in the middle
    /// of a gap the fixed samples cannot fill on their own.
    #[test]
    fn gap_loop_samples_stretch_to_meet_in_the_middle() {
        let gap_bits = 2520;
        let streams: &[u8] = &[
            0x22, 0x08, 0x4e, 0x00, // forward: loop sample 0x4e
            0x22, 0x08, 0x4e, // backward: loop sample 0x4e, then
            0x21, 0x60, 0x22, 0x08, 0x00, 0x00, // 0x60 bits of 0x00
        ];
        let mut area = block_descriptor(
            0,
            gap_bits,
            BLOCK_DESCRIPTOR_LEN,
            BLOCK_FLAG_FWGAP | BLOCK_FLAG_BWGAP,
            0,
            0,
        );
        area.extend_from_slice(streams);
        let track = Track {
            gap_bits: gap_bits as u32,
            track_bits: gap_bits as u32,
            ..Track::default()
        };
        let decoded = only_track(&image(track, &area, ENCODER_SPS_TEST, 2));
        let cells = bits(&decoded);
        assert_eq!(cells.len(), gap_bits);
        assert_mfm_legal(&cells);

        // The nul sample sits against the next block, and 0x4e fills the rest.
        let recovered = data_bits(&cells);
        let (looped, fixed) = recovered.split_at(recovered.len() - 0x60);
        assert!(fixed.iter().all(|bit| !bit));
        assert_eq!(looped.len(), (gap_bits - 2 * 0x60) / 2);

        // The two loops meet in the middle of the gap, which is where the
        // write splice falls: each is aligned to the end it was written from,
        // so the repeated byte picks up a new phase across the join.
        let (forward, backward) = looped.split_at(looped.len() / 2);
        assert_eq!(forward, repeating_data_bits(0x4e, 0, forward.len()));
        let phase = (8 - backward.len() % 8) % 8;
        assert_eq!(backward, repeating_data_bits(0x4e, phase, backward.len()));
    }

    /// The outer cylinders of a real release are left unformatted: they carry
    /// no blocks and no cells, and must read as no medium rather than as a
    /// track of zeroes, which would look like a formatted but empty disk.
    #[test]
    fn unformatted_tracks_hold_no_medium() {
        // Cylinder 0 head 0 is formatted; head 1 is bare, as cylinder 83 is on
        // the disk this was modelled on.
        let bare = Track {
            density: DENSITY_NOISE,
            blocks: 0,
            ..Track::default()
        };
        let mut out = amigados_ipf_image();
        let mut body = imge_body(&bare, 2);
        body[4..8].copy_from_slice(&1u32.to_be_bytes());
        out.extend_from_slice(&chunk(b"IMGE", &body, &[]));
        out.extend_from_slice(&data_chunk(2, &[]));

        let tracks = decode(&out).expect("a partly formatted image is still usable");
        assert!(tracks[0].is_some());
        assert!(tracks[1].is_none());
    }

    #[test]
    fn an_image_with_nothing_formatted_is_rejected() {
        let bare = Track {
            density: DENSITY_NOISE,
            blocks: 0,
            ..Track::default()
        };
        let mut out = chunk(CAPS_SIGNATURE, &[], &[]);
        out.extend_from_slice(&chunk(b"INFO", &info_body(ENCODER_CAPS, 1), &[]));
        out.extend_from_slice(&chunk(b"IMGE", &imge_body(&bare, 1), &[]));
        out.extend_from_slice(&data_chunk(1, &[]));
        let err = decode(&out).expect_err("an image with no formatted track is not usable");
        assert!(format!("{err:#}").contains("no formatted tracks"));
    }

    #[test]
    fn a_corrupt_chunk_is_rejected() {
        let mut image = amigados_ipf_image();
        // Flip a byte inside the INFO chunk body.
        let info = CHUNK_HEADER_LEN + 20;
        image[info] ^= 0xff;
        let err = decode(&image).expect_err("a chunk whose CRC fails should be rejected");
        assert!(format!("{err:#}").contains("CRC mismatch"));
    }

    #[test]
    fn a_corrupt_data_area_is_rejected() {
        let mut image = amigados_ipf_image();
        let last = image.len() - 1;
        image[last] ^= 0xff;
        let err = decode(&image).expect_err("a data area whose CRC fails should be rejected");
        assert!(format!("{err:#}").contains("CRC mismatch"));
    }

    /// The stream must lay down exactly the cells the descriptor promised;
    /// a mismatch means the samples were read the wrong way.
    #[test]
    fn a_block_shorter_than_it_declares_is_rejected() {
        let payload = [0x00u8; 4];
        let mut stream = sample(DATA_DATA, payload.len(), &payload);
        stream.push(0);
        // Claim one byte more than the stream carries.
        let mut area = block_descriptor(payload.len() * 16 + 16, 0, 0, 0, 0, BLOCK_DESCRIPTOR_LEN);
        area.extend_from_slice(&stream);
        let track = Track {
            data_bits: (payload.len() * 16 + 16) as u32,
            track_bits: (payload.len() * 16 + 16) as u32,
            ..Track::default()
        };
        let err = decode(&image(track, &area, ENCODER_CAPS, 1))
            .expect_err("a short block should be rejected");
        assert!(format!("{err:#}").contains("bits, not the"));
    }

    #[test]
    fn a_high_density_signal_is_rejected() {
        let mut image_bytes = amigados_ipf_image();
        // Rewrite the IMGE signal type, then repair the chunk CRC.
        let imge = CHUNK_HEADER_LEN + (CHUNK_HEADER_LEN + INFO_BODY_LEN);
        let body = imge + CHUNK_HEADER_LEN;
        image_bytes[body + 12..body + 16].copy_from_slice(&2u32.to_be_bytes());
        let end = body + IMGE_BODY_LEN;
        image_bytes[imge + 8..imge + 12].copy_from_slice(&[0; 4]);
        let mut crc = Crc::new();
        crc.update(&image_bytes[imge..end]);
        let sum = crc.sum().to_be_bytes();
        image_bytes[imge + 8..imge + 12].copy_from_slice(&sum);

        let err = decode(&image_bytes).expect_err("an HD cell rate is not an Amiga DD disk");
        assert!(format!("{err:#}").contains("signal type"));
    }

    /// Eleven blocks with a gap after each, in the shape of a mastered
    /// protection track, under the given density model.
    fn density_ipf_image(density: u32, gap_bits: usize, start_bit: u32) -> Vec<u8> {
        let payload: Vec<u8> = (0..SECTOR_BYTES).map(|i| (i % 251) as u8).collect();
        let mut stream = sample(DATA_GAP, 2, &[0, 0]);
        stream.extend_from_slice(&sample(DATA_SYNC, 4, &[0x44, 0x89, 0x44, 0x89]));
        stream.extend_from_slice(&sample(DATA_DATA, SECTOR_BYTES, &payload));
        stream.push(0);

        let descriptors = SECTORS * BLOCK_DESCRIPTOR_LEN;
        let mut area = Vec::new();
        for sector in 0..SECTORS {
            area.extend_from_slice(&block_descriptor(
                BLOCK_BITS,
                gap_bits,
                0,
                0,
                0,
                descriptors + sector * stream.len(),
            ));
        }
        // Block 0's gap-value word doubles as the Brierley density key: make
        // it a recognisable bit pattern.
        area[24..28].copy_from_slice(&0x0000_0155u32.to_be_bytes());
        for _ in 0..SECTORS {
            area.extend_from_slice(&stream);
        }

        let track = Track {
            density,
            start_bit,
            data_bits: (SECTORS * BLOCK_BITS) as u32,
            gap_bits: (SECTORS * gap_bits) as u32,
            track_bits: (SECTORS * (BLOCK_BITS + gap_bits)) as u32,
            blocks: SECTORS as u32,
            ..Track::default()
        };
        image(track, &area, ENCODER_CAPS, 1)
    }

    pub(crate) fn copylock_ipf_image() -> Vec<u8> {
        density_ipf_image(DENSITY_COPYLOCK_AMIGA, 720, 1176)
    }

    /// Copylock's key sectors are mastered at other cell rates: the CAPS
    /// profile speeds block 4 by 5.5%, block 5 by 0.5% and slows block 6 by
    /// 4.5%, each run beginning at the gap before its block and ending with
    /// the block's own cells.
    #[test]
    fn copylock_density_profile_covers_the_key_sectors_and_their_leading_gaps() {
        const GAP: usize = 720;
        let block = (BLOCK_BITS + GAP) as u32;
        let track = only_track(&density_ipf_image(DENSITY_COPYLOCK_AMIGA, GAP, 0));
        assert_eq!(
            track.density,
            vec![
                (0, 1000),
                (4 * block - GAP as u32, 945),
                (5 * block - GAP as u32, 995),
                (6 * block - GAP as u32, 1045),
                (6 * block + BLOCK_BITS as u32, 1000),
            ]
        );
        // The profile sits on the revolution as rotated to the index, like
        // the cells do.
        let rotated = only_track(&density_ipf_image(DENSITY_COPYLOCK_AMIGA, GAP, 1176));
        assert_eq!(
            rotated.density,
            vec![
                (0, 1000),
                (4 * block - GAP as u32 + 1176, 945),
                (5 * block - GAP as u32 + 1176, 995),
                (6 * block - GAP as u32 + 1176, 1045),
                (6 * block + BLOCK_BITS as u32 + 1176, 1000),
            ]
        );
    }

    /// The other models weight the block's cells alone; the density-key one
    /// takes a bit per block from block 0's gap value word.
    #[test]
    fn speedlock_and_brierley_density_profiles_weight_the_blocks_alone() {
        const GAP: usize = 720;
        let block = (BLOCK_BITS + GAP) as u32;
        let speedlock = only_track(&density_ipf_image(DENSITY_SPEEDLOCK_AMIGA, GAP, 0));
        assert_eq!(
            speedlock.density,
            vec![
                (0, 1000),
                (block, 1100),
                (block + BLOCK_BITS as u32, 1000),
                (2 * block, 900),
                (2 * block + BLOCK_BITS as u32, 1000),
            ]
        );

        // Key 0x155 = bits 0, 2, 4, 6, 8 set: blocks 1, 3, 5, 7, 9 fast, the
        // rest slow.
        let keyed = only_track(&density_ipf_image(DENSITY_BRIERLEY_AMIGA_KEY, GAP, 0));
        let mut expected = vec![(0u32, 1000u16)];
        for blk in 1..SECTORS as u32 {
            let permille = if blk % 2 == 1 { 950 } else { 1050 };
            expected.push((blk * block, permille));
            expected.push((blk * block + BLOCK_BITS as u32, 1000));
        }
        assert_eq!(keyed.density, expected);
    }

    /// A uniform track has no profile, and a density model this decoder does
    /// not know still decodes -- at the nominal rate throughout.
    #[test]
    fn uniform_and_unknown_density_models_decode_without_a_profile() {
        assert!(only_track(&amigados_ipf_image()).density.is_empty());
        let unknown = only_track(&density_ipf_image(42, 720, 0));
        assert!(unknown.density.is_empty());
        assert_eq!(unknown.bit_len as usize, SECTORS * (BLOCK_BITS + 720));
    }

    /// Files the CAPS encoder wrote spend the flags word on other fields, so
    /// the decoder must not read gap streams out of them.
    const ENCODER_SPS_TEST: u32 = 2;

    #[test]
    fn caps_encoded_files_ignore_the_block_flags() {
        let caps = Info {
            media_type: MEDIA_FLOPPY,
            encoder_type: ENCODER_CAPS,
            encoder_rev: 1,
        };
        assert!(!caps.block_flags_meaningful());
        let sps = Info {
            media_type: MEDIA_FLOPPY,
            encoder_type: ENCODER_SPS_TEST,
            encoder_rev: 2,
        };
        assert!(sps.block_flags_meaningful());
    }
}
