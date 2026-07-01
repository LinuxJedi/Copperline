// SPDX-License-Identifier: GPL-3.0-or-later

//! CD image backend: cue/bin parsing and sector access.
//!
//! Supports single-file and multi-file BIN/CUE layouts with MODE1/2048,
//! MODE1/2352, and AUDIO tracks, including INDEX 00 pregaps stored in
//! the files. INDEX times are file-relative running time at 75 sectors
//! per second regardless of each track's sector size; the disc address
//! space is the concatenation of all files in cue order.

use anyhow::{bail, Context, Result};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

pub const SECTORS_PER_SECOND: u32 = 75;
pub const DATA_SECTOR_BYTES: usize = 2048;
pub const RAW_SECTOR_BYTES: usize = 2352;
/// Standard lead-in offset: LBA 0 is MSF 00:02:00 on a real disc.
pub const LEADIN_SECTORS: u32 = 150;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum TrackKind {
    /// 2048-byte user-data sectors (cooked).
    Mode1_2048,
    /// 2352-byte raw sectors carrying 2048 bytes of user data at +16.
    Mode1_2352,
    /// 2352-byte CD-DA sectors.
    Audio,
}

impl TrackKind {
    pub fn sector_bytes(self) -> usize {
        match self {
            TrackKind::Mode1_2048 => DATA_SECTOR_BYTES,
            TrackKind::Mode1_2352 | TrackKind::Audio => RAW_SECTOR_BYTES,
        }
    }

    pub fn is_data(self) -> bool {
        !matches!(self, TrackKind::Audio)
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CdTrack {
    pub number: u8,
    pub kind: TrackKind,
    /// Disc sector of the track's INDEX 01 (where the TOC points).
    pub start_sector: u32,
    /// Sectors from INDEX 01 to the end of the track's region.
    #[allow(dead_code)]
    pub sector_count: u32,
}

/// A contiguous run of equally-sized sectors inside one image file:
/// one track's region including its in-file INDEX 00 pregap.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct Extent {
    disc_start: u32,
    sector_count: u32,
    sector_bytes: usize,
    file_index: usize,
    byte_offset: u64,
    kind: TrackKind,
}

#[derive(Debug)]
pub struct CdImage {
    files: Vec<File>,
    /// Host path of each entry in `files`, kept so a save state can
    /// reattach the (read-only) image files on load.
    paths: Vec<PathBuf>,
    tracks: Vec<CdTrack>,
    extents: Vec<Extent>,
    total_sectors: u32,
}

/// Serde shadow of `CdImage`: the image files are read-only, so only their
/// paths are stored and deserialization reopens them.
#[derive(serde::Serialize, serde::Deserialize)]
struct CdImageState {
    paths: Vec<PathBuf>,
    tracks: Vec<CdTrack>,
    extents: Vec<Extent>,
    total_sectors: u32,
}

impl serde::Serialize for CdImage {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        CdImageState {
            paths: self.paths.clone(),
            tracks: self.tracks.clone(),
            extents: self.extents.clone(),
            total_sectors: self.total_sectors,
        }
        .serialize(serializer)
    }
}

impl<'de> serde::Deserialize<'de> for CdImage {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let state = CdImageState::deserialize(deserializer)?;
        let mut files = Vec::with_capacity(state.paths.len());
        for path in &state.paths {
            files.push(File::open(path).map_err(|e| {
                serde::de::Error::custom(format!("reopening CD image {}: {e}", path.display()))
            })?);
        }
        Ok(Self {
            files,
            paths: state.paths,
            tracks: state.tracks,
            extents: state.extents,
            total_sectors: state.total_sectors,
        })
    }
}

#[derive(Debug)]
struct RawTrack {
    number: u8,
    kind: TrackKind,
    /// File-relative sector of INDEX 00, when present (pregap start).
    index0: Option<u32>,
    /// File-relative sector of INDEX 01.
    index1: Option<u32>,
    file_index: usize,
}

impl CdImage {
    /// Load a cue sheet and open its BINARY image file(s).
    pub fn load(cue_path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(cue_path)
            .with_context(|| format!("reading cue sheet {}", cue_path.display()))?;
        let dir = cue_path.parent().unwrap_or_else(|| Path::new("."));

        let mut file_names: Vec<String> = Vec::new();
        let mut raw_tracks: Vec<RawTrack> = Vec::new();
        for line in text.lines() {
            let mut words = line.split_whitespace();
            match words.next() {
                Some("FILE") => {
                    let rest = line.trim_start().strip_prefix("FILE").unwrap_or("").trim();
                    let name = rest
                        .strip_suffix("BINARY")
                        .map(str::trim)
                        .unwrap_or(rest)
                        .trim_matches('"');
                    if name.is_empty() {
                        bail!("{}: FILE line has no file name", cue_path.display());
                    }
                    file_names.push(name.to_string());
                }
                Some("TRACK") => {
                    let number: u8 = words
                        .next()
                        .and_then(|s| s.parse().ok())
                        .with_context(|| format!("{}: bad TRACK line", cue_path.display()))?;
                    let kind = match words.next() {
                        Some("MODE1/2048") => TrackKind::Mode1_2048,
                        Some("MODE1/2352") => TrackKind::Mode1_2352,
                        Some("AUDIO") => TrackKind::Audio,
                        other => bail!(
                            "{}: track {} type {:?} is not supported (MODE1/2048, \
                             MODE1/2352, AUDIO)",
                            cue_path.display(),
                            number,
                            other
                        ),
                    };
                    if file_names.is_empty() {
                        bail!("{}: TRACK before FILE", cue_path.display());
                    }
                    raw_tracks.push(RawTrack {
                        number,
                        kind,
                        index0: None,
                        index1: None,
                        file_index: file_names.len() - 1,
                    });
                }
                Some("INDEX") => {
                    let idx: u8 = words.next().and_then(|s| s.parse().ok()).unwrap_or(0xFF);
                    let msf = words
                        .next()
                        .with_context(|| format!("{}: INDEX without time", cue_path.display()))?;
                    let sector = parse_msf(msf)
                        .with_context(|| format!("{}: bad INDEX time {msf}", cue_path.display()))?;
                    let last = raw_tracks
                        .last_mut()
                        .with_context(|| format!("{}: INDEX before TRACK", cue_path.display()))?;
                    match idx {
                        0 => last.index0 = Some(sector),
                        1 => last.index1 = Some(sector),
                        _ => {}
                    }
                }
                // CATALOG / PERFORMER / TITLE / REM etc. are ignored.
                _ => {}
            }
        }
        if raw_tracks.is_empty() {
            bail!("{}: no tracks", cue_path.display());
        }
        for track in &raw_tracks {
            if track.index1.is_none() {
                bail!(
                    "{}: track {} has no INDEX 01",
                    cue_path.display(),
                    track.number
                );
            }
        }

        let mut files = Vec::with_capacity(file_names.len());
        let mut paths = Vec::with_capacity(file_names.len());
        let mut file_lens = Vec::with_capacity(file_names.len());
        for name in &file_names {
            let path = dir.join(name);
            let file = File::open(&path)
                .with_context(|| format!("opening CD image {}", path.display()))?;
            let len = file
                .metadata()
                .with_context(|| format!("stat {}", path.display()))?
                .len();
            files.push(file);
            paths.push(path);
            file_lens.push(len);
        }

        // Lay the files out back to back on the disc. Within one file,
        // each track's region runs from its INDEX 00 (or INDEX 01) to
        // the next track's region start or the file's end; sector sizes
        // may differ per track, so byte offsets accumulate region by
        // region and the file size must come out exact.
        let mut tracks = Vec::with_capacity(raw_tracks.len());
        let mut extents: Vec<Extent> = Vec::with_capacity(raw_tracks.len());
        let mut disc = 0u32;
        let mut i = 0usize;
        while i < raw_tracks.len() {
            let file_index = raw_tracks[i].file_index;
            let mut in_file = i;
            while in_file < raw_tracks.len() && raw_tracks[in_file].file_index == file_index {
                in_file += 1;
            }
            let file_tracks = &raw_tracks[i..in_file];
            let file_len = file_lens[file_index];
            let mut byte_offset = 0u64;
            for (j, track) in file_tracks.iter().enumerate() {
                let region_start = track.index0.unwrap_or_else(|| track.index1.unwrap());
                let index1 = track.index1.unwrap();
                if index1 < region_start {
                    bail!(
                        "{}: track {} INDEX 01 before INDEX 00",
                        cue_path.display(),
                        track.number
                    );
                }
                let region_end_sector = match file_tracks.get(j + 1) {
                    Some(next) => next.index0.unwrap_or_else(|| next.index1.unwrap()),
                    None => {
                        // Earlier tracks' INDEX values come straight from
                        // the (untrusted) cue sheet; a corrupt/crafted one
                        // can claim more bytes than the file actually has,
                        // which would underflow this subtraction.
                        let Some(remaining) = file_len.checked_sub(byte_offset) else {
                            bail!(
                                "{}: track {} starts {} bytes past the end of {}",
                                cue_path.display(),
                                track.number,
                                byte_offset.saturating_sub(file_len),
                                file_names[file_index],
                            );
                        };
                        if !remaining.is_multiple_of(track.kind.sector_bytes() as u64) {
                            bail!(
                                "{}: track {} does not end on a sector boundary",
                                cue_path.display(),
                                track.number
                            );
                        }
                        region_start + (remaining / track.kind.sector_bytes() as u64) as u32
                    }
                };
                if region_end_sector < index1 {
                    bail!(
                        "{}: track {} INDEX times are not monotonic",
                        cue_path.display(),
                        track.number
                    );
                }
                let region_sectors = region_end_sector - region_start;
                extents.push(Extent {
                    disc_start: disc,
                    sector_count: region_sectors,
                    sector_bytes: track.kind.sector_bytes(),
                    file_index,
                    byte_offset,
                    kind: track.kind,
                });
                tracks.push(CdTrack {
                    number: track.number,
                    kind: track.kind,
                    start_sector: disc + (index1 - region_start),
                    sector_count: region_sectors - (index1 - region_start),
                });
                disc += region_sectors;
                byte_offset += u64::from(region_sectors) * track.kind.sector_bytes() as u64;
            }
            if byte_offset != file_len {
                bail!(
                    "{}: cue layout covers {} bytes of {} but the file is {} bytes",
                    cue_path.display(),
                    byte_offset,
                    file_names[file_index],
                    file_len
                );
            }
            i = in_file;
        }

        Ok(Self {
            files,
            paths,
            tracks,
            extents,
            total_sectors: disc,
        })
    }

    pub fn tracks(&self) -> &[CdTrack] {
        &self.tracks
    }

    /// Total size of the disc in sectors.
    pub fn total_sectors(&self) -> u32 {
        self.total_sectors
    }

    fn extent_for_sector(&self, sector: u32) -> Option<&Extent> {
        self.extents
            .iter()
            .find(|e| sector >= e.disc_start && sector < e.disc_start + e.sector_count)
    }

    fn read_raw(&mut self, extent_index: usize, sector: u32, buf: &mut [u8]) -> Result<()> {
        let extent = &self.extents[extent_index];
        let in_extent = u64::from(sector - extent.disc_start);
        let offset = extent.byte_offset + in_extent * extent.sector_bytes as u64;
        let file = &mut self.files[extent.file_index];
        file.seek(SeekFrom::Start(offset))?;
        file.read_exact(buf)?;
        Ok(())
    }

    /// Read the 2048 bytes of user data in a data sector. Fails on audio
    /// tracks.
    pub fn read_data_sector(
        &mut self,
        sector: u32,
        buf: &mut [u8; DATA_SECTOR_BYTES],
    ) -> Result<()> {
        let (index, kind) = {
            let extent = self
                .extent_for_sector(sector)
                .with_context(|| format!("sector {sector} beyond end of disc"))?;
            if !extent.kind.is_data() {
                bail!("sector {sector} is in an audio track");
            }
            (
                self.extents
                    .iter()
                    .position(|e| std::ptr::eq(e, extent))
                    .unwrap(),
                extent.kind,
            )
        };
        match kind {
            TrackKind::Mode1_2048 => self.read_raw(index, sector, buf),
            TrackKind::Mode1_2352 => {
                let mut raw = [0u8; RAW_SECTOR_BYTES];
                self.read_raw(index, sector, &mut raw)?;
                buf.copy_from_slice(&raw[16..16 + DATA_SECTOR_BYTES]);
                Ok(())
            }
            TrackKind::Audio => unreachable!(),
        }
    }

    /// Read one 2352-byte raw sector from an audio track.
    pub fn read_audio_sector(
        &mut self,
        sector: u32,
        buf: &mut [u8; RAW_SECTOR_BYTES],
    ) -> Result<()> {
        let index = {
            let extent = self
                .extent_for_sector(sector)
                .with_context(|| format!("sector {sector} beyond end of disc"))?;
            if extent.kind.is_data() {
                bail!("sector {sector} is in a data track");
            }
            self.extents
                .iter()
                .position(|e| std::ptr::eq(e, extent))
                .unwrap()
        };
        self.read_raw(index, sector, buf)
    }

    /// Read one full 2352-byte raw frame at `sector`, whatever the track
    /// type: raw images are copied through; cooked (2048-byte) data
    /// sectors get a synthesized sync + BCD MSF header (zero EDC/ECC).
    pub fn read_raw_sector(&mut self, sector: u32, buf: &mut [u8; RAW_SECTOR_BYTES]) -> Result<()> {
        let (index, kind) = {
            let extent = self
                .extent_for_sector(sector)
                .with_context(|| format!("sector {sector} beyond end of disc"))?;
            (
                self.extents
                    .iter()
                    .position(|e| std::ptr::eq(e, extent))
                    .unwrap(),
                extent.kind,
            )
        };
        match kind {
            TrackKind::Mode1_2352 | TrackKind::Audio => self.read_raw(index, sector, buf),
            TrackKind::Mode1_2048 => {
                let mut data = [0u8; DATA_SECTOR_BYTES];
                self.read_raw(index, sector, &mut data)?;
                buf.fill(0);
                buf[1..11].fill(0xFF);
                let msf = sector + LEADIN_SECTORS;
                buf[12] = to_bcd((msf / (60 * 75)) as u8);
                buf[13] = to_bcd(((msf / 75) % 60) as u8);
                buf[14] = to_bcd((msf % 75) as u8);
                buf[15] = 1; // mode 1
                buf[16..16 + DATA_SECTOR_BYTES].copy_from_slice(&data);
                Ok(())
            }
        }
    }

    /// Whether `sector` falls inside an audio track region.
    pub fn is_audio_sector(&self, sector: u32) -> bool {
        self.extent_for_sector(sector)
            .is_some_and(|e| !e.kind.is_data())
    }

    /// One-line TOC summary for the log.
    pub fn describe(&self) -> String {
        let data = self.tracks.iter().filter(|t| t.kind.is_data()).count();
        let audio = self.tracks.len() - data;
        format!(
            "{} tracks ({} data, {} audio), {} sectors",
            self.tracks.len(),
            data,
            audio,
            self.total_sectors()
        )
    }
}

pub(crate) fn to_bcd(v: u8) -> u8 {
    ((v / 10) << 4) | (v % 10)
}

/// Parse a cue MM:SS:FF time to a sector number.
fn parse_msf(s: &str) -> Result<u32> {
    let parts: Vec<&str> = s.split(':').collect();
    if parts.len() != 3 {
        bail!("expected MM:SS:FF");
    }
    let mm: u32 = parts[0].parse()?;
    let ss: u32 = parts[1].parse()?;
    let ff: u32 = parts[2].parse()?;
    if ss >= 60 || ff >= SECTORS_PER_SECOND {
        bail!("out-of-range MSF field");
    }
    Ok((mm * 60 + ss) * SECTORS_PER_SECOND + ff)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::path::PathBuf;

    fn temp_path(name: &str) -> PathBuf {
        static UNIQUE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let unique = UNIQUE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "copperline-cd-{}-{unique}-{name}",
            std::process::id()
        ))
    }

    fn write_file(path: &Path, bytes: &[u8]) {
        let mut f = File::create(path).unwrap();
        f.write_all(bytes).unwrap();
    }

    #[test]
    fn parses_msf_times() {
        assert_eq!(parse_msf("00:00:00").unwrap(), 0);
        assert_eq!(parse_msf("00:32:49").unwrap(), 32 * 75 + 49);
        assert_eq!(parse_msf("62:03:74").unwrap(), (62 * 60 + 3) * 75 + 74);
        assert!(parse_msf("00:60:00").is_err());
        assert!(parse_msf("00:00:75").is_err());
    }

    #[test]
    fn single_file_mixed_mode_layout_round_trips() {
        let cue = temp_path("mixed.cue");
        let bin = temp_path("mixed.bin");
        // 4 data sectors at 2048 bytes, then 2 audio sectors at 2352.
        let mut bytes = Vec::new();
        for s in 0..4u8 {
            bytes.extend(std::iter::repeat_n(s, DATA_SECTOR_BYTES));
        }
        for s in 0..2u8 {
            bytes.extend(std::iter::repeat_n(0xA0 + s, RAW_SECTOR_BYTES));
        }
        write_file(&bin, &bytes);
        std::fs::write(
            &cue,
            format!(
                "FILE \"{}\" BINARY\n  TRACK 01 MODE1/2048\n    INDEX 01 00:00:00\n  TRACK 02 AUDIO\n    INDEX 01 00:00:04\n",
                bin.file_name().unwrap().to_string_lossy()
            ),
        )
        .unwrap();
        let mut image = CdImage::load(&cue).unwrap();
        assert_eq!(image.tracks().len(), 2);
        assert_eq!(image.total_sectors(), 6);
        assert_eq!(image.tracks()[1].start_sector, 4);

        let mut data = [0u8; DATA_SECTOR_BYTES];
        image.read_data_sector(3, &mut data).unwrap();
        assert!(data.iter().all(|&b| b == 3));
        assert!(image.read_data_sector(4, &mut data).is_err());

        let mut audio = [0u8; RAW_SECTOR_BYTES];
        image.read_audio_sector(5, &mut audio).unwrap();
        assert!(audio.iter().all(|&b| b == 0xA1));
        assert!(image.is_audio_sector(5));
        assert!(!image.is_audio_sector(2));

        let _ = std::fs::remove_file(&cue);
        let _ = std::fs::remove_file(&bin);
    }

    #[test]
    fn serde_reopens_image_files_and_serves_same_sectors() {
        let cue = temp_path("serde.cue");
        let bin = temp_path("serde.bin");
        let mut bytes = Vec::new();
        for s in 0..4u8 {
            bytes.extend(std::iter::repeat_n(s, DATA_SECTOR_BYTES));
        }
        write_file(&bin, &bytes);
        std::fs::write(
            &cue,
            format!(
                "FILE \"{}\" BINARY\n  TRACK 01 MODE1/2048\n    INDEX 01 00:00:00\n",
                bin.file_name().unwrap().to_string_lossy()
            ),
        )
        .unwrap();
        let mut image = CdImage::load(&cue).unwrap();

        let encoded = bincode::serialize(&image).unwrap();
        let mut restored: CdImage = bincode::deserialize(&encoded).unwrap();
        assert_eq!(restored.total_sectors(), image.total_sectors());
        assert_eq!(restored.tracks().len(), image.tracks().len());
        let mut a = [0u8; DATA_SECTOR_BYTES];
        let mut b = [0u8; DATA_SECTOR_BYTES];
        for sector in 0..4 {
            image.read_data_sector(sector, &mut a).unwrap();
            restored.read_data_sector(sector, &mut b).unwrap();
            assert_eq!(a, b, "sector {sector}");
        }

        // A missing image file must fail the load with the path named,
        // not deserialize into an image that panics on first read.
        let _ = std::fs::remove_file(&bin);
        let err = bincode::deserialize::<CdImage>(&encoded)
            .expect_err("deserializing with the image file gone must fail");
        assert!(err.to_string().contains("reopening CD image"));

        let _ = std::fs::remove_file(&cue);
    }

    #[test]
    fn raw_data_track_skips_sector_header() {
        let cue = temp_path("raw.cue");
        let bin = temp_path("raw.bin");
        let mut sector = vec![0u8; RAW_SECTOR_BYTES];
        for (i, b) in sector.iter_mut().enumerate() {
            *b = if i < 16 { 0xEE } else { 0x42 };
        }
        write_file(&bin, &sector);
        std::fs::write(
            &cue,
            format!(
                "FILE \"{}\" BINARY\n  TRACK 01 MODE1/2352\n    INDEX 01 00:00:00\n",
                bin.file_name().unwrap().to_string_lossy()
            ),
        )
        .unwrap();
        let mut image = CdImage::load(&cue).unwrap();
        let mut data = [0u8; DATA_SECTOR_BYTES];
        image.read_data_sector(0, &mut data).unwrap();
        assert!(data.iter().all(|&b| b == 0x42));
        let _ = std::fs::remove_file(&cue);
        let _ = std::fs::remove_file(&bin);
    }

    #[test]
    fn multi_file_cue_with_pregap_lays_out_disc_addresses() {
        // Track 1: data, 3 raw sectors in its own file. Track 2: audio
        // with a 2-sector in-file pregap (INDEX 00) plus 4 sectors of
        // content. Track 3: audio, 2 sectors.
        let cue = temp_path("multi.cue");
        let bin1 = temp_path("t1.bin");
        let bin2 = temp_path("t2.bin");
        let bin3 = temp_path("t3.bin");
        write_file(&bin1, &vec![0x11u8; 3 * RAW_SECTOR_BYTES]);
        write_file(&bin2, &vec![0x22u8; 6 * RAW_SECTOR_BYTES]);
        write_file(&bin3, &vec![0x33u8; 2 * RAW_SECTOR_BYTES]);
        std::fs::write(
            &cue,
            format!(
                concat!(
                    "CATALOG 0000000000000\n",
                    "FILE \"{}\" BINARY\n  TRACK 01 MODE1/2352\n    INDEX 01 00:00:00\n",
                    "FILE \"{}\" BINARY\n  TRACK 02 AUDIO\n    INDEX 00 00:00:00\n    INDEX 01 00:00:02\n",
                    "FILE \"{}\" BINARY\n  TRACK 03 AUDIO\n    INDEX 01 00:00:00\n",
                ),
                bin1.file_name().unwrap().to_string_lossy(),
                bin2.file_name().unwrap().to_string_lossy(),
                bin3.file_name().unwrap().to_string_lossy(),
            ),
        )
        .unwrap();
        let mut image = CdImage::load(&cue).unwrap();
        assert_eq!(image.total_sectors(), 3 + 6 + 2);
        let tracks = image.tracks();
        assert_eq!(tracks[0].start_sector, 0);
        assert_eq!(tracks[0].sector_count, 3);
        // Track 2's INDEX 01 lands after the 2-sector pregap.
        assert_eq!(tracks[1].start_sector, 5);
        assert_eq!(tracks[1].sector_count, 4);
        assert_eq!(tracks[2].start_sector, 9);
        assert_eq!(tracks[2].sector_count, 2);

        // Reads address the disc across files; the pregap is readable
        // as part of track 2's region.
        let mut audio = [0u8; RAW_SECTOR_BYTES];
        image.read_audio_sector(3, &mut audio).unwrap(); // pregap
        assert!(audio.iter().all(|&b| b == 0x22));
        image.read_audio_sector(9, &mut audio).unwrap();
        assert!(audio.iter().all(|&b| b == 0x33));
        let mut data = [0u8; DATA_SECTOR_BYTES];
        image.read_data_sector(2, &mut data).unwrap();
        assert!(data.iter().all(|&b| b == 0x11));

        let _ = std::fs::remove_file(&cue);
        let _ = std::fs::remove_file(&bin1);
        let _ = std::fs::remove_file(&bin2);
        let _ = std::fs::remove_file(&bin3);
    }

    #[test]
    fn track_index_claiming_more_than_the_file_holds_is_rejected_not_a_panic() {
        // Track 1's INDEX 01 (used as track 2's region start, i.e. track
        // 1's region end) claims a sector far beyond what the shared file
        // actually contains. That inflates `byte_offset` past `file_len`
        // by the time the last track computes its remaining-bytes span,
        // which must be rejected as a malformed cue sheet instead of
        // underflowing the subtraction.
        let cue = temp_path("overrun.cue");
        let bin = temp_path("overrun.bin");
        write_file(&bin, &vec![0u8; 2 * RAW_SECTOR_BYTES]);
        std::fs::write(
            &cue,
            format!(
                concat!(
                    "FILE \"{}\" BINARY\n  TRACK 01 AUDIO\n    INDEX 01 00:00:00\n",
                    "  TRACK 02 AUDIO\n    INDEX 01 00:10:00\n",
                ),
                bin.file_name().unwrap().to_string_lossy(),
            ),
        )
        .unwrap();
        let err = CdImage::load(&cue).expect_err("overrunning cue sheet must be rejected");
        assert!(
            err.to_string().contains("past the end of"),
            "unexpected error: {err}"
        );
        let _ = std::fs::remove_file(&cue);
        let _ = std::fs::remove_file(&bin);
    }

    #[test]
    fn size_mismatch_is_rejected() {
        let cue = temp_path("short.cue");
        let bin = temp_path("short.bin");
        write_file(&bin, &vec![0u8; DATA_SECTOR_BYTES + 7]);
        std::fs::write(
            &cue,
            format!(
                "FILE \"{}\" BINARY\n  TRACK 01 MODE1/2048\n    INDEX 01 00:00:00\n",
                bin.file_name().unwrap().to_string_lossy()
            ),
        )
        .unwrap();
        let err = CdImage::load(&cue).unwrap_err();
        assert!(err.to_string().contains("sector boundary"), "{err:#}");
        let _ = std::fs::remove_file(&cue);
        let _ = std::fs::remove_file(&bin);
    }
}
