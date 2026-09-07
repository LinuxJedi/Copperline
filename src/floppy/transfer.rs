// SPDX-License-Identifier: GPL-3.0-or-later

//! Lossless disk media for desktop netplay. UAE extended ADF cannot carry
//! mastered density profiles or flux cell times. This versioned container
//! carries decoded tracks only, with no paths or controller/save-state data.

use super::formats::{DensitySpan, FloppyImage, FloppyImageData, FloppyTrackImage};
use super::{BYTES_PER_SECTOR, MAX_EXTENDED_TRACKS};
use anyhow::{bail, ensure, Context, Result};

pub(super) const SIGNATURE: &[u8; 8] = b"CLFLOP01";
const ABSENT: u32 = u32::MAX;

pub(super) fn encode(image: &FloppyImage, limit: usize) -> Result<Vec<u8>> {
    let tracks = match &image.data {
        FloppyImageData::StandardAdf(bytes) => {
            ensure!(
                bytes.len() <= limit,
                "floppy transfer exceeds {limit} bytes"
            );
            return Ok(bytes.clone());
        }
        FloppyImageData::Tracks(tracks) => tracks,
    };
    ensure!(
        tracks.len() <= MAX_EXTENDED_TRACKS,
        "too many floppy tracks"
    );
    let mut out = Writer {
        bytes: Vec::new(),
        limit,
    };
    out.put(SIGNATURE)?;
    out.put(&[
        u8::from(image.write_protected),
        u8::from(image.legacy_extended_adf),
    ])?;
    out.put(&(tracks.len() as u16).to_le_bytes())?;
    for track in tracks {
        match track {
            None => out.put(&[0])?,
            Some(FloppyTrackImage::AmigaDos(bytes)) => {
                out.put(&[1])?;
                out.len(bytes.len())?;
                out.put(bytes)?;
            }
            Some(FloppyTrackImage::RawMfm {
                words,
                bit_len,
                stored_len,
                revolutions,
                legacy_sync,
                bitcell_ns,
                density,
            }) => {
                out.put(&[2])?;
                out.put(&bit_len.to_le_bytes())?;
                out.len(*stored_len)?;
                out.put(&[*revolutions, u8::from(legacy_sync.is_some())])?;
                if let Some(sync) = legacy_sync {
                    out.put(&sync.to_le_bytes())?;
                }
                out.len(words.len())?;
                for word in words {
                    out.put(&word.to_le_bytes())?;
                }
                out.optional_len(bitcell_ns.as_deref())?;
                if let Some(cells) = bitcell_ns {
                    for ns in cells {
                        out.put(&ns.to_le_bytes())?;
                    }
                }
                out.optional_len(density.as_deref())?;
                if let Some(spans) = density {
                    for span in spans {
                        out.put(&span.start_bit.to_le_bytes())?;
                        out.put(&span.permille.to_le_bytes())?;
                    }
                }
            }
        }
    }
    Ok(out.bytes)
}

pub(super) fn decode(bytes: &[u8]) -> Result<(FloppyImageData, bool, bool)> {
    let mut input = Reader(bytes);
    ensure!(
        input.take(8)? == SIGNATURE,
        "invalid floppy transfer version"
    );
    let protected = input.flag()?;
    let legacy = input.flag()?;
    let count = input.u16()? as usize;
    ensure!(count <= MAX_EXTENDED_TRACKS, "too many floppy tracks");
    let mut tracks = Vec::with_capacity(count);
    for _ in 0..count {
        tracks.push(match input.byte()? {
            0 => None,
            1 => {
                let bytes = input.array(1)?;
                ensure!(
                    bytes.len().is_multiple_of(BYTES_PER_SECTOR),
                    "unaligned floppy sectors"
                );
                Some(FloppyTrackImage::AmigaDos(bytes.to_vec()))
            }
            2 => {
                let bit_len = input.u32()?;
                let stored_len = input.u32()? as usize;
                let revolutions = input.byte()?;
                ensure!(revolutions > 0, "floppy track has no revolutions");
                let legacy_sync = if input.flag()? {
                    Some(input.u16()?)
                } else {
                    None
                };
                let payload = input.array(2)?;
                ensure!(
                    stored_len <= payload.len(),
                    "floppy stored length exceeds data"
                );
                ensure!(
                    u64::from(bit_len) <= payload.len() as u64 * 8,
                    "floppy bit length exceeds data"
                );
                let words = payload
                    .chunks_exact(2)
                    .map(|b| u16::from_le_bytes([b[0], b[1]]))
                    .collect();
                let bitcell_ns = input
                    .optional_array(4)?
                    .map(|bytes| bytes.chunks_exact(4).map(le_u32).collect::<Vec<_>>());
                if let Some(cells) = &bitcell_ns {
                    ensure!(
                        cells.len() as u64 <= payload.len() as u64 * 8,
                        "too many floppy cell times"
                    );
                    ensure!(cells.iter().all(|&ns| ns > 0), "zero floppy cell time");
                }
                let density = input.optional_array(6)?.map(|bytes| {
                    bytes
                        .chunks_exact(6)
                        .map(|b| DensitySpan {
                            start_bit: le_u32(&b[..4]),
                            permille: u16::from_le_bytes([b[4], b[5]]),
                        })
                        .collect::<Vec<_>>()
                });
                if let Some(spans) = &density {
                    ensure!(
                        spans
                            .iter()
                            .all(|s| s.start_bit < bit_len && s.permille > 0),
                        "invalid floppy density span"
                    );
                    ensure!(
                        spans.windows(2).all(|s| s[0].start_bit < s[1].start_bit),
                        "unordered floppy density spans"
                    );
                }
                Some(FloppyTrackImage::RawMfm {
                    words,
                    bit_len,
                    stored_len,
                    revolutions,
                    legacy_sync,
                    bitcell_ns,
                    density,
                })
            }
            _ => bail!("invalid floppy track type"),
        });
    }
    ensure!(input.0.is_empty(), "trailing floppy transfer data");
    Ok((FloppyImageData::Tracks(tracks), protected, legacy))
}

struct Writer {
    bytes: Vec<u8>,
    limit: usize,
}

impl Writer {
    fn put(&mut self, bytes: &[u8]) -> Result<()> {
        ensure!(
            bytes.len() <= self.limit - self.bytes.len(),
            "floppy transfer exceeds {} bytes",
            self.limit
        );
        self.bytes.extend_from_slice(bytes);
        Ok(())
    }

    fn len(&mut self, len: usize) -> Result<()> {
        let len = u32::try_from(len).context("floppy array is too large")?;
        ensure!(len != ABSENT, "floppy array is too large");
        self.put(&len.to_le_bytes())
    }

    fn optional_len<T>(&mut self, items: Option<&[T]>) -> Result<()> {
        match items {
            Some(items) => self.len(items.len()),
            None => self.put(&ABSENT.to_le_bytes()),
        }
    }
}

struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    fn take(&mut self, len: usize) -> Result<&'a [u8]> {
        ensure!(len <= self.0.len(), "truncated floppy transfer");
        let (bytes, rest) = self.0.split_at(len);
        self.0 = rest;
        Ok(bytes)
    }
    fn byte(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }
    fn flag(&mut self) -> Result<bool> {
        let flag = self.byte()?;
        ensure!(flag <= 1, "invalid floppy flag");
        Ok(flag == 1)
    }
    fn u16(&mut self) -> Result<u16> {
        let b = self.take(2)?;
        Ok(u16::from_le_bytes([b[0], b[1]]))
    }
    fn u32(&mut self) -> Result<u32> {
        Ok(le_u32(self.take(4)?))
    }
    fn counted(&mut self, count: u32, width: usize) -> Result<&'a [u8]> {
        // Check the complete byte range before allocating any array, including
        // on wasm32. Counts never become unchecked capacities or loop bounds.
        let len = (count as usize)
            .checked_mul(width)
            .context("floppy array is too large")?;
        self.take(len)
    }
    fn array(&mut self, width: usize) -> Result<&'a [u8]> {
        let count = self.u32()?;
        self.counted(count, width)
    }
    fn optional_array(&mut self, width: usize) -> Result<Option<&'a [u8]>> {
        let count = self.u32()?;
        if count == ABSENT {
            Ok(None)
        } else {
            Ok(Some(self.counted(count, width)?))
        }
    }
}

fn le_u32(bytes: &[u8]) -> u32 {
    u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
}
