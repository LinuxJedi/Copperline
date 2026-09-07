// SPDX-License-Identifier: GPL-3.0-or-later

//! Floppy image container and format decoders: standard and extended ADF,
//! SCP flux images, IPF delegation, and gzip/zip containers.

use super::*;
use crate::config::FloppyDriveConfig;
use crate::{dms, gzip, ipf};
use anyhow::{bail, ensure, Context, Result};
use flate2::read::DeflateDecoder;
use flate2::CrcReader;
use log::debug;
use std::io::{Cursor, Read};
use std::path::{Path, PathBuf};

// Allow large multi-revolution flux captures while bounding gzip expansion.
// Network callers impose their smaller transfer limit on the expanded image.
pub(super) const GZIP_IMAGE_LIMIT: usize = 128 * 1024 * 1024;

#[derive(serde::Serialize, serde::Deserialize)]
pub(super) struct FloppyImage {
    pub(super) path: PathBuf,
    pub(super) data: FloppyImageData,
    pub(super) write_protected: bool,
    pub(super) legacy_extended_adf: bool,
    pub(super) backing: FloppyImageBacking,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub(super) enum FloppyImageBacking {
    File,
    Memory,
}

#[derive(serde::Serialize, serde::Deserialize)]
pub(super) enum FloppyImageData {
    StandardAdf(Vec<u8>),
    Tracks(Vec<Option<FloppyTrackImage>>),
}

#[derive(serde::Serialize, serde::Deserialize)]
pub(super) enum FloppyTrackImage {
    AmigaDos(Vec<u8>),
    RawMfm {
        words: Vec<u16>,
        bit_len: u32,
        stored_len: usize,
        revolutions: u8,
        legacy_sync: Option<u16>,
        bitcell_ns: Option<Vec<u32>>,
        /// The cell-rate profile of a mastered protection track (IPF density
        /// models), as the runs where the rate changes; `None` is uniform.
        density: Option<Vec<DensitySpan>>,
    },
}

/// From `start_bit` until the next span (or the index), cells are written at
/// `permille` / 1000 of the nominal cell time.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct DensitySpan {
    pub(crate) start_bit: u32,
    pub(crate) permille: u16,
}

impl FloppyImage {
    pub(super) fn load(config: &FloppyDriveConfig) -> Result<Self> {
        let packed = std::fs::read(&config.path)
            .with_context(|| format!("reading floppy image {}", config.path.display()))?;
        Self::from_bytes_with_backing(
            packed,
            config.path.clone(),
            config.write_protected,
            FloppyImageBacking::File,
            GZIP_IMAGE_LIMIT,
        )
    }

    /// Decode an already-loaded image (the byte-for-byte file contents:
    /// ADF/extended ADF/DMS/SCP/IPF, optionally gzip- or zip-packed) without
    /// touching the filesystem. `path` is the display/write-back label.
    pub(crate) fn from_bytes(
        packed: Vec<u8>,
        path: PathBuf,
        write_protected: bool,
    ) -> Result<Self> {
        Self::from_bytes_with_backing(
            packed,
            path,
            write_protected,
            FloppyImageBacking::File,
            GZIP_IMAGE_LIMIT,
        )
    }

    pub(super) fn from_memory_bytes(
        packed: Vec<u8>,
        path: PathBuf,
        write_protected: bool,
        expanded_limit: usize,
    ) -> Result<Self> {
        if packed.starts_with(super::transfer::SIGNATURE) {
            ensure!(
                packed.len() <= expanded_limit,
                "floppy transfer exceeds byte limit"
            );
            let (data, protected, legacy_extended_adf) = super::transfer::decode(&packed)?;
            return Ok(Self {
                path,
                data,
                write_protected: write_protected || protected,
                legacy_extended_adf,
                backing: FloppyImageBacking::Memory,
            });
        }
        Self::from_bytes_with_backing(
            packed,
            path,
            write_protected,
            FloppyImageBacking::Memory,
            expanded_limit,
        )
    }

    fn from_bytes_with_backing(
        packed: Vec<u8>,
        path: PathBuf,
        write_protected: bool,
        backing: FloppyImageBacking,
        expanded_limit: usize,
    ) -> Result<Self> {
        let (data, write_protected, legacy_extended_adf) = if packed.starts_with(GZIP_SIGNATURE) {
            let unpacked = decode_gzip_floppy_image(&packed, expanded_limit)?;
            decode_floppy_payload(unpacked, true, &path)?
        } else if packed.starts_with(ZIP_SIGNATURE) {
            ensure!(
                ADF_SIZE <= expanded_limit,
                "expanded floppy image exceeds {expanded_limit} bytes"
            );
            let unpacked = decode_zip_floppy_image(&packed)?;
            decode_floppy_payload(unpacked, true, &path)?
        } else {
            decode_floppy_payload(packed, write_protected, &path)?
        };

        Ok(Self {
            path,
            data,
            write_protected,
            legacy_extended_adf,
            backing,
        })
    }

    pub(super) fn track_stream(&self, track: usize) -> Option<TrackStream> {
        match &self.data {
            FloppyImageData::StandardAdf(adf) => {
                Some(synthetic_track_stream(encode_adf_track(track, adf)))
            }
            FloppyImageData::Tracks(tracks) => {
                let image_track = tracks.get(track)?.as_ref()?;
                match image_track {
                    FloppyTrackImage::AmigaDos(data) => {
                        Some(synthetic_track_stream(encode_amigados_track(track, data)))
                    }
                    FloppyTrackImage::RawMfm {
                        words,
                        bit_len,
                        revolutions,
                        legacy_sync,
                        density,
                        ..
                    } => Some(raw_mfm_track_stream(
                        words,
                        *bit_len,
                        *revolutions,
                        legacy_sync.is_some(),
                        density.as_deref().unwrap_or(&[]),
                    )),
                }
            }
        }
    }
}

/// A synthetic (ADF / AmigaDOS) track: one perfectly-aligned revolution of
/// uniform 2 us cells. The uniform `word_cck` keeps the DMA word cadence and
/// every timing assertion identical to the old word-grid model.
pub(super) fn synthetic_track_stream(words: Vec<u16>) -> TrackStream {
    let word_cck = FloppyController::word_cck_for_track_words(words.len());
    let bit_len = words.len() * 16;
    TrackStream {
        revs: vec![TrackRev::new(words, bit_len, word_cck)],
    }
}

/// A flux/raw-MFM track: split the stored stream into its captured revolutions,
/// each with its exact `bit_len`, so the looping head sees no word-rounding
/// seam at the index and weak/fuzzy bits vary per revolution.
pub(super) fn raw_mfm_track_stream(
    words: &[u16],
    bit_len: u32,
    revolutions: u8,
    legacy_sync: bool,
    density: &[DensitySpan],
) -> TrackStream {
    let rev_bits = (bit_len as usize).max(1);
    let words_per_rev = rev_bits.div_ceil(16).max(1);
    let rev_count = if legacy_sync {
        1
    } else {
        (revolutions.max(1) as usize)
            .min(words.len() / words_per_rev)
            .max(1)
    };

    let mut revs = Vec::with_capacity(rev_count);
    for r in 0..rev_count {
        let start = r * words_per_rev;
        let end = (start + words_per_rev).min(words.len());
        if start >= end {
            break;
        }
        let rev_words = words[start..end].to_vec();
        let this_bits = rev_bits.min(rev_words.len() * 16);
        let word_cck = FloppyController::word_cck_for_track_words(rev_words.len());
        // The mastered cell-rate profile is a property of the track, so every
        // captured revolution of it carries the same one.
        revs.push(TrackRev::with_density(
            rev_words, this_bits, word_cck, density,
        ));
    }
    if revs.is_empty() {
        let word_cck = FloppyController::word_cck_for_track_words(words.len());
        revs.push(TrackRev::with_density(
            words.to_vec(),
            words.len() * 16,
            word_cck,
            density,
        ));
    }
    TrackStream { revs }
}

pub(super) fn decode_floppy_payload(
    packed: Vec<u8>,
    config_write_protected: bool,
    path: &Path,
) -> Result<(FloppyImageData, bool, bool)> {
    let legacy_extended_adf = packed.starts_with(UAE_EXT1_SIGNATURE);
    let (data, write_protected) = if packed.len() == ADF_SIZE {
        (FloppyImageData::StandardAdf(packed), config_write_protected)
    } else if packed.starts_with(UAE_EXT2_SIGNATURE) {
        (decode_uae_extended_adf(&packed)?, config_write_protected)
    } else if packed.starts_with(UAE_EXT1_SIGNATURE) {
        (
            decode_uae_legacy_extended_adf(&packed)?,
            config_write_protected,
        )
    } else if dms::is_dms(&packed) {
        let data = dms::decode_dms_adf(&packed)
            .with_context(|| format!("decoding DMS {}", path.display()))?;
        (FloppyImageData::StandardAdf(data), true)
    } else if ipf::is_ipf(&packed) {
        // IPF preserves the written track rather than its sectors, so there is
        // nothing to write back into: it is always read-only.
        (
            decode_ipf_image(&packed)
                .with_context(|| format!("decoding IPF {}", path.display()))?,
            true,
        )
    } else if packed.starts_with(SCP_SIGNATURE) {
        (decode_scp_flux_image(&packed)?, true)
    } else {
        bail!(
            "floppy image {} is {} bytes; expected {} bytes (ADF), gzip-compressed supported image, UAE extended ADF, IPF, SCP, or DMS",
            path.display(),
            packed.len(),
            ADF_SIZE
        );
    };
    Ok((data, write_protected, legacy_extended_adf))
}

pub(super) fn decode_gzip_floppy_image(data: &[u8], limit: usize) -> Result<Vec<u8>> {
    ensure!(data.starts_with(GZIP_SIGNATURE), "missing gzip signature");
    // Every member, not just the first: a concatenated ADZ would otherwise
    // decode to a fraction of the disk, which the format dispatch could only
    // report as an unknown format rather than as the half-read image it is.
    let unpacked = gzip::inflate_members(data, Some(limit as u64))
        .context("decompressing gzip-compressed floppy image")?;
    ensure!(
        unpacked.len() <= limit,
        "expanded floppy image exceeds {limit} bytes"
    );
    Ok(unpacked)
}

pub(super) fn decode_zip_floppy_image(data: &[u8]) -> Result<Vec<u8>> {
    ensure!(data.starts_with(ZIP_SIGNATURE), "missing zip signature");
    let mut cursor = Cursor::new(data);
    let mut u16buf = [0u8; 2];
    let mut u32buf = [0u8; 4];

    // Skip to compression method (at offset 8 in local file header)
    cursor.set_position(8);
    cursor.read_exact(&mut u16buf)?;
    let compression = u16::from_le_bytes(u16buf);

    // Skip to CRC-32 (at offset 14 in local file header)
    cursor.set_position(14);
    cursor.read_exact(&mut u32buf)?;
    let expected_crc = u32::from_le_bytes(u32buf);

    // Skip to uncompressed size (at offset 22 in local file header)
    cursor.set_position(22);
    cursor.read_exact(&mut u32buf)?;
    let uncomp_size = u32::from_le_bytes(u32buf);
    ensure!(
        uncomp_size as usize == ADF_SIZE,
        "invalid ADF file size in ZIP archive"
    );

    // Skip to file name length and extra field length
    cursor.set_position(26);
    cursor.read_exact(&mut u16buf)?;
    let file_name_length = u16::from_le_bytes(u16buf);
    cursor.read_exact(&mut u16buf)?;
    let extra_field_length = u16::from_le_bytes(u16buf);

    // Skip the file name and extra field to reach the compressed data
    cursor.set_position((30 + file_name_length + extra_field_length) as u64);

    let mut decompressed = vec![0; ADF_SIZE];

    let calculated_crc = match compression {
        8 => {
            // Deflate compression
            let mut decode_reader = CrcReader::new(DeflateDecoder::new(cursor));
            decode_reader
                .read_exact(&mut decompressed)
                .context("deflating zipped floppy image")?;
            decode_reader.crc().sum()
        }
        0 => {
            // No compression
            let mut reader = CrcReader::new(cursor);
            reader
                .read_exact(&mut decompressed)
                .context("unarchiving zipped floppy image")?;
            reader.crc().sum()
        }
        n => {
            bail!("Unsupported compression method in zip archive: {n}");
        }
    };
    // Verify CRC32
    if calculated_crc != expected_crc {
        bail!("checksum error in zip archive: expected: {expected_crc} != calculated: {calculated_crc}");
    }

    Ok(decompressed)
}

pub(super) fn decode_uae_extended_adf(data: &[u8]) -> Result<FloppyImageData> {
    ensure!(data.len() >= 12, "UAE extended ADF header is truncated");
    ensure!(
        data.starts_with(UAE_EXT2_SIGNATURE),
        "missing UAE-1ADF signature"
    );
    let tracks = u16::from_be_bytes([data[10], data[11]]) as usize;
    ensure!(
        tracks <= MAX_EXTENDED_TRACKS,
        "UAE extended ADF has {tracks} tracks, max supported is {MAX_EXTENDED_TRACKS}"
    );
    let header_len = 12 + tracks * 12;
    ensure!(
        data.len() >= header_len,
        "UAE extended ADF track table is truncated"
    );

    let mut offset = header_len;
    let mut out = Vec::with_capacity(tracks);
    for track in 0..tracks {
        let desc = &data[12 + track * 12..12 + (track + 1) * 12];
        let revolutions = desc[2].saturating_add(1);
        let track_type = desc[3];
        let len = u32::from_be_bytes([desc[4], desc[5], desc[6], desc[7]]) as usize;
        let bit_len = u32::from_be_bytes([desc[8], desc[9], desc[10], desc[11]]);
        ensure!(
            offset + len <= data.len(),
            "UAE extended ADF track {track} data is truncated"
        );
        let payload = &data[offset..offset + len];
        offset += len;

        let image_track = match track_type {
            0 => {
                if len == 0 {
                    None
                } else {
                    decode_uae_extended_amigados_payload(track, payload, bit_len)?
                        .map(|sector_data| FloppyTrackImage::AmigaDos(sector_data.to_vec()))
                }
            }
            1 => Some(FloppyTrackImage::RawMfm {
                words: raw_mfm_words(track, payload, bit_len)?,
                bit_len: if bit_len == 0 {
                    (len * 8) as u32
                } else {
                    bit_len
                },
                stored_len: len,
                revolutions,
                legacy_sync: None,
                bitcell_ns: None,
                density: None,
            }),
            other => {
                ensure!(
                    len == 0,
                    "unsupported UAE extended ADF track {track} type {other}"
                );
                None
            }
        };
        if revolutions > 1 && matches!(image_track, Some(FloppyTrackImage::RawMfm { .. })) {
            debug!(
                "UAE extended ADF raw track {track} has {revolutions} stored revolutions; preserving cyclic raw stream"
            );
        }
        out.push(image_track);
    }
    Ok(FloppyImageData::Tracks(out))
}

pub(super) fn decode_uae_extended_amigados_payload(
    track: usize,
    payload: &[u8],
    bit_len: u32,
) -> Result<Option<&[u8]>> {
    let data_len = if bit_len == 0 {
        payload.len()
    } else {
        ensure!(
            bit_len.is_multiple_of(8),
            "UAE extended ADF track {track} AmigaDOS bit length is not byte-aligned"
        );
        (bit_len / 8) as usize
    };
    ensure!(
        data_len <= payload.len(),
        "UAE extended ADF track {track} AmigaDOS bit length exceeds stored data"
    );
    // UAE-1ADF exporters can store type-0 DOS tracks at raw-track byte
    // length. `bit_len` identifies the sector payload; any surplus is fill.
    // Extra blank cylinders may also appear as all-zero raw-length type-0
    // tracks, which carry no sector stream.
    if !data_len.is_multiple_of(BYTES_PER_SECTOR) && payload.iter().all(|&byte| byte == 0) {
        return Ok(None);
    }
    ensure!(
        data_len.is_multiple_of(BYTES_PER_SECTOR),
        "UAE extended ADF track {track} AmigaDOS data is not sector-aligned"
    );
    ensure!(
        payload[data_len..].iter().all(|&byte| byte == 0),
        "UAE extended ADF track {track} AmigaDOS padding after bit length is non-zero"
    );
    Ok(Some(&payload[..data_len]))
}

pub(super) fn decode_uae_legacy_extended_adf(data: &[u8]) -> Result<FloppyImageData> {
    ensure!(data.len() >= 8 + 160 * 4, "UAE--ADF header is truncated");
    ensure!(
        data.starts_with(UAE_EXT1_SIGNATURE),
        "missing UAE--ADF signature"
    );
    let mut offset = 8 + 160 * 4;
    let mut out = Vec::with_capacity(160);
    for track in 0..160 {
        let desc = &data[8 + track * 4..8 + (track + 1) * 4];
        let sync = u16::from_be_bytes([desc[0], desc[1]]);
        let len = u16::from_be_bytes([desc[2], desc[3]]) as usize;
        ensure!(
            offset + len <= data.len(),
            "UAE--ADF track {track} data is truncated"
        );
        let payload = &data[offset..offset + len];
        offset += len;
        if len == 0 {
            out.push(None);
        } else if sync == 0 {
            ensure!(
                len.is_multiple_of(BYTES_PER_SECTOR),
                "UAE--ADF track {track} AmigaDOS data is not sector-aligned"
            );
            out.push(Some(FloppyTrackImage::AmigaDos(payload.to_vec())));
        } else {
            let mut words = Vec::with_capacity(len / 2 + 1);
            words.push(sync);
            words.extend(raw_mfm_words(track, payload, (len * 8) as u32)?);
            out.push(Some(FloppyTrackImage::RawMfm {
                words,
                bit_len: (len * 8 + 16) as u32,
                stored_len: len,
                revolutions: 1,
                legacy_sync: Some(sync),
                bitcell_ns: None,
                density: None,
            }));
        }
    }
    Ok(FloppyImageData::Tracks(out))
}

/// An IPF stores the encoded track itself, so each of its tracks arrives as a
/// finished revolution of MFM -- the same shape a flux capture takes, and read
/// back through the same path.
pub(super) fn decode_ipf_image(data: &[u8]) -> Result<FloppyImageData> {
    let tracks = ipf::decode(data)?
        .into_iter()
        .map(|track| {
            track.map(|track| FloppyTrackImage::RawMfm {
                stored_len: track.words.len() * 2,
                words: track.words,
                bit_len: track.bit_len,
                // The format describes one canonical revolution, and its cell
                // rate is the nominal 2 us that `word_cck_for_track_words`
                // already paces a raw track at -- except where the track's
                // density model marks sectors mastered at another rate.
                revolutions: 1,
                legacy_sync: None,
                bitcell_ns: None,
                density: (!track.density.is_empty()).then(|| {
                    track
                        .density
                        .iter()
                        .map(|&(start_bit, permille)| DensitySpan {
                            start_bit,
                            permille,
                        })
                        .collect()
                }),
            })
        })
        .collect();
    Ok(FloppyImageData::Tracks(tracks))
}

pub(super) fn decode_scp_flux_image(data: &[u8]) -> Result<FloppyImageData> {
    ensure!(data.len() >= 0x10, "SCP image header is truncated");
    ensure!(data.starts_with(SCP_SIGNATURE), "missing SCP signature");
    let flags = data[0x08];
    verify_scp_checksum(data)?;
    ensure!(
        scp_flux_width_is_16_bit(data[0x09]),
        "SCP flux entry width {} is not supported",
        data[0x09]
    );
    let track_table_offset = scp_track_table_offset(flags);
    ensure!(
        data.len() >= track_table_offset + SCP_TRACK_TABLE_LEN,
        "SCP track header table is truncated"
    );

    let revolutions = data[0x05] as usize;
    ensure!(revolutions > 0, "SCP image has no revolutions");
    let start_track = data[0x06] as usize;
    let end_track = data[0x07] as usize;
    ensure!(
        start_track <= end_track && end_track < SCP_TRACKS,
        "SCP track range {start_track}..={end_track} is invalid"
    );
    let flux_resolution_ns = SCP_CAPTURE_BASE_NS;

    let mut tracks: Vec<Option<FloppyTrackImage>> = (0..SCP_TRACKS).map(|_| None).collect();
    for track in start_track..=end_track {
        let table_off = track_table_offset + track * 4;
        let tdh_offset = read_le_u32(&data[table_off..table_off + 4]) as usize;
        if tdh_offset == 0 {
            continue;
        }
        ensure!(
            tdh_offset < data.len(),
            "SCP track {track} header offset is outside the image"
        );
        tracks[track] = Some(
            decode_scp_track(
                data,
                track,
                tdh_offset,
                revolutions,
                flags,
                flux_resolution_ns,
            )
            .with_context(|| format!("decoding SCP track {track}"))?,
        );
    }

    Ok(FloppyImageData::Tracks(tracks))
}

pub(super) const SCP_DEFAULT_16_BIT_FLUX_WIDTH: u8 = 0;
pub(super) const SCP_EXPLICIT_16_BIT_FLUX_WIDTH: u8 = 16;
pub(super) fn scp_flux_width_is_16_bit(width: u8) -> bool {
    matches!(
        width,
        SCP_DEFAULT_16_BIT_FLUX_WIDTH | SCP_EXPLICIT_16_BIT_FLUX_WIDTH
    )
}

pub(super) fn verify_scp_checksum(data: &[u8]) -> Result<()> {
    let expected = read_le_u32(&data[SCP_CHECKSUM_OFFSET..SCP_CHECKSUM_OFFSET + 4]);
    if expected == 0 {
        return Ok(());
    }
    let actual = scp_checksum(data);
    ensure!(
        actual == expected,
        "SCP checksum mismatch: expected {expected:08X}, got {actual:08X}"
    );
    Ok(())
}

pub(super) fn scp_checksum(data: &[u8]) -> u32 {
    data.get(SCP_CHECKSUM_START..)
        .unwrap_or_default()
        .iter()
        .fold(0u32, |sum, &byte| sum.wrapping_add(u32::from(byte)))
}

pub(super) fn scp_track_table_offset(flags: u8) -> usize {
    if flags & SCP_FLAG_EXTENDED_MODE != 0 {
        SCP_EXTENDED_TRACK_TABLE_OFFSET
    } else {
        SCP_TRACK_TABLE_OFFSET
    }
}

pub(super) fn decode_scp_track(
    data: &[u8],
    track: usize,
    tdh_offset: usize,
    revolutions: usize,
    flags: u8,
    flux_resolution_ns: u64,
) -> Result<FloppyTrackImage> {
    let header_len = 4 + revolutions * 12;
    let header_end = tdh_offset
        .checked_add(header_len)
        .context("SCP track header offset overflow")?;
    ensure!(
        header_end <= data.len(),
        "SCP track {track} header is truncated"
    );
    let header = &data[tdh_offset..header_end];
    ensure!(
        &header[0..3] == b"TRK",
        "SCP track {track} is missing TRK header"
    );
    ensure!(
        header[3] as usize == track,
        "SCP track header number {} does not match table entry {track}",
        header[3]
    );

    let mut target_bit_len = None;
    let mut decoded_revolutions = 0u8;
    let mut all_words = Vec::new();
    let mut all_bitcell_ns = Vec::new();
    for rev in 0..revolutions {
        let entry = 4 + rev * 12;
        let index_time = read_le_u32(&header[entry..entry + 4]);
        let flux_entries = read_le_u32(&header[entry + 4..entry + 8]);
        let data_offset = read_le_u32(&header[entry + 8..entry + 12]) as usize;
        if flux_entries == 0 {
            continue;
        }
        ensure!(
            data_offset >= header_len,
            "SCP track {track} revolution {rev} flux data overlaps the track header"
        );

        let flux_bytes = (flux_entries as usize)
            .checked_mul(2)
            .context("SCP flux data length overflow")?;
        let flux_start = tdh_offset
            .checked_add(data_offset)
            .context("SCP flux data offset overflow")?;
        let flux_end = flux_start
            .checked_add(flux_bytes)
            .context("SCP flux data end overflow")?;
        ensure!(
            flux_end <= data.len(),
            "SCP track {track} revolution {rev} flux data is truncated"
        );

        let rev_target = match target_bit_len {
            Some(bits) => Some(bits),
            None => scp_revolution_bit_len(index_time, flags)?,
        };
        let (words, bit_len, bitcell_ns) = scp_flux_to_mfm_words(
            track,
            rev,
            &data[flux_start..flux_end],
            flux_resolution_ns,
            rev_target,
        )?;
        let target = *target_bit_len.get_or_insert(bit_len);
        ensure!(
            bit_len == target,
            "SCP track {track} revolution {rev} bit length {bit_len} does not match first revolution {target}"
        );
        all_words.extend(words);
        all_bitcell_ns.extend(bitcell_ns);
        decoded_revolutions = decoded_revolutions.saturating_add(1);
    }

    ensure!(
        decoded_revolutions > 0,
        "SCP track {track} has no flux data"
    );
    let bit_len = target_bit_len.unwrap_or(0);
    let stored_len = all_words.len() * 2;
    Ok(FloppyTrackImage::RawMfm {
        words: all_words,
        bit_len,
        stored_len,
        revolutions: decoded_revolutions,
        legacy_sync: None,
        bitcell_ns: Some(all_bitcell_ns),
        density: None,
    })
}

pub(super) fn scp_revolution_bit_len(index_time: u32, flags: u8) -> Result<Option<u32>> {
    if flags & SCP_FLAG_INDEX == 0 {
        let ns = if flags & SCP_FLAG_RPM_360 != 0 {
            SCP_360_RPM_REV_NS
        } else {
            SCP_300_RPM_REV_NS
        };
        return scp_bit_len_from_ns(ns).map(Some);
    }

    if index_time == 0 {
        Ok(None)
    } else {
        scp_bit_len_from_ns(u64::from(index_time) * SCP_CAPTURE_BASE_NS).map(Some)
    }
}

pub(super) fn scp_bit_len_from_ns(ns: u64) -> Result<u32> {
    let bits = ((ns + AMIGA_DD_BITCELL_NS / 2) / AMIGA_DD_BITCELL_NS).max(1);
    ensure!(
        bits <= u64::from(MAX_SCP_REVOLUTION_BITS),
        "SCP revolution bit length {bits} exceeds supported limit {MAX_SCP_REVOLUTION_BITS}"
    );
    Ok(bits as u32)
}

pub(super) fn scp_flux_to_mfm_words(
    track: usize,
    rev: usize,
    flux: &[u8],
    flux_resolution_ns: u64,
    target_bit_len: Option<u32>,
) -> Result<(Vec<u16>, u32, Vec<u32>)> {
    ensure!(
        flux.len().is_multiple_of(2),
        "SCP track {track} revolution {rev} has odd flux byte length"
    );
    let bit_cap = target_bit_len.unwrap_or(MAX_SCP_REVOLUTION_BITS);
    let capped_by_index = target_bit_len.is_some();
    let mut words = Vec::new();
    let mut bitcell_ns = Vec::new();
    let mut bit_len = 0u32;
    let mut overflow_ticks = 0u64;
    // PLL data separator: recover MFM cells from flux intervals, locking the
    // cell-time estimate onto the local flux rate. For each interval the cell
    // count is `round(interval / cell)`; the estimate is then nudged toward the
    // measured per-cell time. A flux transition is a "1" cell preceded by
    // (n-1) "0" cells. This avoids the cumulative drift a fixed 2 us grid
    // accumulates when the disk's true rate differs from nominal.
    let mut cell_ns = AMIGA_DD_BITCELL_NS as f64;
    for chunk in flux.chunks_exact(2) {
        let ticks = u64::from(read_be_u16(chunk));
        if ticks == 0 {
            overflow_ticks = overflow_ticks.saturating_add(65_536);
            continue;
        }
        let total_ticks = overflow_ticks.saturating_add(ticks);
        overflow_ticks = 0;
        let interval_ns = total_ticks
            .checked_mul(flux_resolution_ns)
            .context("SCP flux interval overflows nanoseconds")? as f64;
        let cells = (interval_ns / cell_ns).round().max(1.0);
        let measured = interval_ns / cells;
        cell_ns += (measured - cell_ns) * SCP_PLL_GAIN;
        cell_ns = cell_ns.clamp(SCP_PLL_MIN_CELL_NS, SCP_PLL_MAX_CELL_NS);
        let per_cell_ns = measured.round().clamp(1.0, u32::MAX as f64) as u32;
        let cells = cells as u64;
        append_scp_cells(
            &mut words,
            &mut bitcell_ns,
            &mut bit_len,
            cells.saturating_sub(1),
            false,
            bit_cap,
            capped_by_index,
            per_cell_ns,
        )
        .with_context(|| format!("SCP track {track} revolution {rev} flux interval"))?;
        append_scp_cells(
            &mut words,
            &mut bitcell_ns,
            &mut bit_len,
            1,
            true,
            bit_cap,
            capped_by_index,
            per_cell_ns,
        )
        .with_context(|| format!("SCP track {track} revolution {rev} flux interval"))?;
    }
    if overflow_ticks != 0 && !capped_by_index {
        // Trailing no-flux gap before the index hole: pad with idle cells at
        // the current recovered rate.
        let interval_ns = overflow_ticks
            .checked_mul(flux_resolution_ns)
            .context("SCP flux silence overflows nanoseconds")? as f64;
        let cells = (interval_ns / cell_ns).round().max(0.0) as u64;
        append_scp_cells(
            &mut words,
            &mut bitcell_ns,
            &mut bit_len,
            cells,
            false,
            bit_cap,
            capped_by_index,
            cell_ns.round() as u32,
        )
        .with_context(|| format!("SCP track {track} revolution {rev} trailing flux overflow"))?;
    }
    if let Some(target) = target_bit_len {
        // SCP gives no per-cell timing after the last transition; the
        // synthetic index-padding cells retain nominal DD timing.
        let padding_cells = u64::from(target.saturating_sub(bit_len));
        append_scp_cells(
            &mut words,
            &mut bitcell_ns,
            &mut bit_len,
            padding_cells,
            false,
            target,
            true,
            AMIGA_DD_BITCELL_NS as u32,
        )?;
    }
    ensure!(
        bit_len > 0,
        "SCP track {track} revolution {rev} produced an empty bit stream"
    );
    Ok((words, bit_len, bitcell_ns))
}

pub(super) fn append_scp_cells(
    words: &mut Vec<u16>,
    bitcell_ns: &mut Vec<u32>,
    bit_len: &mut u32,
    cells: u64,
    bit: bool,
    bit_cap: u32,
    capped_by_index: bool,
    cell_ns: u32,
) -> Result<()> {
    let available = u64::from(bit_cap.saturating_sub(*bit_len));
    if cells > available && !capped_by_index {
        bail!("SCP flux stream exceeds supported bit length {MAX_SCP_REVOLUTION_BITS}");
    }
    for _ in 0..cells.min(available) {
        push_mfm_bit(words, bit_len, bit);
        bitcell_ns.push(cell_ns);
    }
    Ok(())
}

pub(super) fn push_mfm_bit(words: &mut Vec<u16>, bit_len: &mut u32, bit: bool) {
    if (*bit_len).is_multiple_of(16) {
        words.push(0);
    }
    if bit {
        let bit_pos = 15 - (*bit_len % 16);
        if let Some(word) = words.last_mut() {
            *word |= 1 << bit_pos;
        }
    }
    *bit_len = bit_len.saturating_add(1);
}

pub(super) fn read_le_u32(bytes: &[u8]) -> u32 {
    u32::from_le_bytes(bytes.try_into().unwrap())
}

pub(super) fn read_be_u16(bytes: &[u8]) -> u16 {
    u16::from_be_bytes(bytes.try_into().unwrap())
}

pub(super) fn raw_mfm_words(track: usize, payload: &[u8], bit_len: u32) -> Result<Vec<u16>> {
    let effective_bit_len = if bit_len == 0 {
        (payload.len() * 8) as u32
    } else {
        bit_len
    };
    ensure!(
        effective_bit_len as usize <= payload.len() * 8,
        "raw MFM track {track} bit length exceeds stored bytes"
    );
    Ok(payload
        .chunks(2)
        .map(|chunk| u16::from_be_bytes([chunk[0], *chunk.get(1).unwrap_or(&0)]))
        .collect())
}
