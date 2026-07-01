// SPDX-License-Identifier: GPL-3.0-or-later

//! DMS (Disk Masher System) floppy archive decompression.
//!
//! This is a Rust port of the decompression path from xDMS 1.3.2,
//! whose original source is public domain. DMS stores Amiga floppy
//! data as compressed cylinder records; the emulator consumes the
//! decoded standard 901,120-byte ADF byte stream.

use crate::floppy::ADF_SIZE;
use anyhow::{anyhow, bail, ensure, Context, Result};

const HEAD_LEN: usize = 56;
const TRACK_HEAD_LEN: usize = 20;
const DMS_TRACKS: usize = 80;
#[cfg(test)]
const DMS_CYLINDER_BYTES: usize = ADF_SIZE / DMS_TRACKS;

const D_CODE: [u8; 256] = [
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01,
    0x02, 0x02, 0x02, 0x02, 0x02, 0x02, 0x02, 0x02, 0x02, 0x02, 0x02, 0x02, 0x02, 0x02, 0x02, 0x02,
    0x03, 0x03, 0x03, 0x03, 0x03, 0x03, 0x03, 0x03, 0x03, 0x03, 0x03, 0x03, 0x03, 0x03, 0x03, 0x03,
    0x04, 0x04, 0x04, 0x04, 0x04, 0x04, 0x04, 0x04, 0x05, 0x05, 0x05, 0x05, 0x05, 0x05, 0x05, 0x05,
    0x06, 0x06, 0x06, 0x06, 0x06, 0x06, 0x06, 0x06, 0x07, 0x07, 0x07, 0x07, 0x07, 0x07, 0x07, 0x07,
    0x08, 0x08, 0x08, 0x08, 0x08, 0x08, 0x08, 0x08, 0x09, 0x09, 0x09, 0x09, 0x09, 0x09, 0x09, 0x09,
    0x0A, 0x0A, 0x0A, 0x0A, 0x0A, 0x0A, 0x0A, 0x0A, 0x0B, 0x0B, 0x0B, 0x0B, 0x0B, 0x0B, 0x0B, 0x0B,
    0x0C, 0x0C, 0x0C, 0x0C, 0x0D, 0x0D, 0x0D, 0x0D, 0x0E, 0x0E, 0x0E, 0x0E, 0x0F, 0x0F, 0x0F, 0x0F,
    0x10, 0x10, 0x10, 0x10, 0x11, 0x11, 0x11, 0x11, 0x12, 0x12, 0x12, 0x12, 0x13, 0x13, 0x13, 0x13,
    0x14, 0x14, 0x14, 0x14, 0x15, 0x15, 0x15, 0x15, 0x16, 0x16, 0x16, 0x16, 0x17, 0x17, 0x17, 0x17,
    0x18, 0x18, 0x19, 0x19, 0x1A, 0x1A, 0x1B, 0x1B, 0x1C, 0x1C, 0x1D, 0x1D, 0x1E, 0x1E, 0x1F, 0x1F,
    0x20, 0x20, 0x21, 0x21, 0x22, 0x22, 0x23, 0x23, 0x24, 0x24, 0x25, 0x25, 0x26, 0x26, 0x27, 0x27,
    0x28, 0x28, 0x29, 0x29, 0x2A, 0x2A, 0x2B, 0x2B, 0x2C, 0x2C, 0x2D, 0x2D, 0x2E, 0x2E, 0x2F, 0x2F,
    0x30, 0x31, 0x32, 0x33, 0x34, 0x35, 0x36, 0x37, 0x38, 0x39, 0x3A, 0x3B, 0x3C, 0x3D, 0x3E, 0x3F,
];

const D_LEN: [u8; 256] = [
    0x03, 0x03, 0x03, 0x03, 0x03, 0x03, 0x03, 0x03, 0x03, 0x03, 0x03, 0x03, 0x03, 0x03, 0x03, 0x03,
    0x03, 0x03, 0x03, 0x03, 0x03, 0x03, 0x03, 0x03, 0x03, 0x03, 0x03, 0x03, 0x03, 0x03, 0x03, 0x03,
    0x04, 0x04, 0x04, 0x04, 0x04, 0x04, 0x04, 0x04, 0x04, 0x04, 0x04, 0x04, 0x04, 0x04, 0x04, 0x04,
    0x04, 0x04, 0x04, 0x04, 0x04, 0x04, 0x04, 0x04, 0x04, 0x04, 0x04, 0x04, 0x04, 0x04, 0x04, 0x04,
    0x04, 0x04, 0x04, 0x04, 0x04, 0x04, 0x04, 0x04, 0x04, 0x04, 0x04, 0x04, 0x04, 0x04, 0x04, 0x04,
    0x05, 0x05, 0x05, 0x05, 0x05, 0x05, 0x05, 0x05, 0x05, 0x05, 0x05, 0x05, 0x05, 0x05, 0x05, 0x05,
    0x05, 0x05, 0x05, 0x05, 0x05, 0x05, 0x05, 0x05, 0x05, 0x05, 0x05, 0x05, 0x05, 0x05, 0x05, 0x05,
    0x05, 0x05, 0x05, 0x05, 0x05, 0x05, 0x05, 0x05, 0x05, 0x05, 0x05, 0x05, 0x05, 0x05, 0x05, 0x05,
    0x05, 0x05, 0x05, 0x05, 0x05, 0x05, 0x05, 0x05, 0x05, 0x05, 0x05, 0x05, 0x05, 0x05, 0x05, 0x05,
    0x05, 0x06, 0x06, 0x06, 0x06, 0x06, 0x06, 0x06, 0x06, 0x06, 0x06, 0x06, 0x06, 0x06, 0x06, 0x06,
    0x06, 0x06, 0x06, 0x06, 0x06, 0x06, 0x06, 0x06, 0x06, 0x06, 0x06, 0x06, 0x06, 0x06, 0x06, 0x06,
    0x06, 0x06, 0x06, 0x06, 0x06, 0x06, 0x06, 0x06, 0x06, 0x06, 0x06, 0x06, 0x06, 0x06, 0x06, 0x06,
    0x06, 0x06, 0x07, 0x07, 0x07, 0x07, 0x07, 0x07, 0x07, 0x07, 0x07, 0x07, 0x07, 0x07, 0x07, 0x07,
    0x07, 0x07, 0x07, 0x07, 0x07, 0x07, 0x07, 0x07, 0x07, 0x07, 0x07, 0x07, 0x07, 0x07, 0x07, 0x07,
    0x07, 0x07, 0x07, 0x07, 0x07, 0x07, 0x07, 0x07, 0x07, 0x07, 0x07, 0x07, 0x07, 0x07, 0x07, 0x07,
    0x07, 0x07, 0x07, 0x08, 0x08, 0x08, 0x08, 0x08, 0x08, 0x08, 0x08, 0x08, 0x08, 0x08, 0x08, 0x08,
];

pub fn is_dms(data: &[u8]) -> bool {
    data.starts_with(b"DMS!")
}

pub fn decode_dms_adf(data: &[u8]) -> Result<Vec<u8>> {
    ensure!(data.len() >= HEAD_LEN, "DMS archive is too short");
    ensure!(is_dms(data), "missing DMS! signature");

    let expected_header_crc = be_u16(&data[HEAD_LEN - 2..HEAD_LEN]);
    let actual_header_crc = dms_crc16(&data[4..HEAD_LEN - 2]);
    ensure!(
        expected_header_crc == actual_header_crc,
        "DMS info header CRC mismatch: expected {expected_header_crc:04X}, got {actual_header_crc:04X}"
    );

    let geninfo = be_u16(&data[10..12]);
    ensure!(
        geninfo & 0x0002 == 0,
        "encrypted DMS archives are not supported"
    );
    let disktype = be_u16(&data[50..52]);
    ensure!(disktype != 7, "FMS DMS archives are not floppy images");

    let mut pos = HEAD_LEN;
    let mut decoder = Decrunchers::new();
    decoder.init();
    let mut adf = Vec::with_capacity(ADF_SIZE);

    while pos < data.len() {
        let Some(header) = data.get(pos..pos + TRACK_HEAD_LEN) else {
            break;
        };
        if &header[0..2] != b"TR" {
            break;
        }
        let track = TrackHeader::parse(header)?;
        pos += TRACK_HEAD_LEN;
        let packed = data.get(pos..pos + track.packed_len).ok_or_else(|| {
            anyhow!(
                "DMS track {} packed data truncated: needs {} bytes at offset {}",
                track.number,
                track.packed_len,
                pos
            )
        })?;
        pos += track.packed_len;

        let actual_data_crc = dms_crc16(packed);
        ensure!(
            actual_data_crc == track.data_crc,
            "DMS track {} packed CRC mismatch: expected {:04X}, got {:04X}",
            track.number,
            track.data_crc,
            actual_data_crc
        );

        if track.number < DMS_TRACKS as u16 && track.unpacked_len > 2048 {
            let unpacked = decoder
                .unpack_track(packed, &track)
                .with_context(|| format!("unpacking DMS track {}", track.number))?;
            let actual_sum = checksum16(&unpacked);
            ensure!(
                actual_sum == track.unpacked_sum,
                "DMS track {} unpacked checksum mismatch: expected {:04X}, got {:04X}",
                track.number,
                track.unpacked_sum,
                actual_sum
            );
            adf.extend_from_slice(&unpacked);
        } else if track.number == 80 || track.number == 0xFFFF {
            // FILEID.DIZ and banner records are intentionally skipped.
        }
    }

    ensure!(
        adf.len() == ADF_SIZE,
        "DMS decoded to {} bytes, expected {} bytes",
        adf.len(),
        ADF_SIZE
    );
    Ok(adf)
}

#[derive(Debug)]
struct TrackHeader {
    number: u16,
    packed_len: usize,
    stage_len: usize,
    unpacked_len: usize,
    flags: u8,
    compression: u8,
    unpacked_sum: u16,
    data_crc: u16,
}

impl TrackHeader {
    fn parse(data: &[u8]) -> Result<Self> {
        ensure!(data.len() == TRACK_HEAD_LEN, "bad DMS track header length");
        let expected_crc = be_u16(&data[18..20]);
        let actual_crc = dms_crc16(&data[..18]);
        ensure!(
            expected_crc == actual_crc,
            "DMS track header CRC mismatch: expected {expected_crc:04X}, got {actual_crc:04X}"
        );
        Ok(Self {
            number: be_u16(&data[2..4]),
            packed_len: be_u16(&data[6..8]) as usize,
            stage_len: be_u16(&data[8..10]) as usize,
            unpacked_len: be_u16(&data[10..12]) as usize,
            flags: data[12],
            compression: data[13],
            unpacked_sum: be_u16(&data[14..16]),
            data_crc: be_u16(&data[16..18]),
        })
    }
}

struct Decrunchers {
    text: Vec<u8>,
    quick_text_loc: u16,
    medium_text_loc: u16,
    deep_text_loc: u16,
    heavy_text_loc: u16,
    deep: DeepState,
    heavy: HeavyState,
}

impl Decrunchers {
    fn new() -> Self {
        Self {
            text: vec![0; 32_000],
            quick_text_loc: 0,
            medium_text_loc: 0,
            deep_text_loc: 0,
            heavy_text_loc: 0,
            deep: DeepState::default(),
            heavy: HeavyState::default(),
        }
    }

    fn init(&mut self) {
        self.text.fill(0);
        self.quick_text_loc = 251;
        self.medium_text_loc = 0x3FBE;
        self.heavy_text_loc = 0;
        self.deep_text_loc = 0x3FC4;
        self.deep.initialized = false;
    }

    fn unpack_track(&mut self, packed: &[u8], track: &TrackHeader) -> Result<Vec<u8>> {
        let unpacked = match track.compression {
            0 => {
                ensure!(
                    packed.len() >= track.unpacked_len,
                    "uncompressed DMS track is shorter than its output length"
                );
                packed[..track.unpacked_len].to_vec()
            }
            1 => unpack_rle(packed, track.unpacked_len)?,
            2 => {
                let stage = self.unpack_quick(packed, track.stage_len)?;
                unpack_rle(&stage, track.unpacked_len)?
            }
            3 => {
                let stage = self.unpack_medium(packed, track.stage_len)?;
                unpack_rle(&stage, track.unpacked_len)?
            }
            4 => {
                let stage = self.unpack_deep(packed, track.stage_len)?;
                unpack_rle(&stage, track.unpacked_len)?
            }
            5 | 6 => {
                let heavy_flags = if track.compression == 5 {
                    track.flags & 7
                } else {
                    track.flags | 8
                };
                let stage = self.unpack_heavy(packed, heavy_flags, track.stage_len)?;
                if track.flags & 4 != 0 {
                    unpack_rle(&stage, track.unpacked_len)?
                } else {
                    ensure!(
                        stage.len() == track.unpacked_len,
                        "heavy DMS track decoded to {} bytes before RLE, expected {}",
                        stage.len(),
                        track.unpacked_len
                    );
                    stage
                }
            }
            other => bail!("unsupported DMS compression mode {other}"),
        };

        if track.flags & 1 == 0 {
            self.init();
        }
        Ok(unpacked)
    }

    fn unpack_quick(&mut self, packed: &[u8], output_len: usize) -> Result<Vec<u8>> {
        let mut bits = BitReader::new(packed);
        let mut out = Vec::with_capacity(output_len);
        while out.len() < output_len {
            if bits.take(1) != 0 {
                let byte = bits.take(8) as u8;
                out.push(byte);
                self.text[(self.quick_text_loc & 0x00FF) as usize] = byte;
                self.quick_text_loc = self.quick_text_loc.wrapping_add(1);
            } else {
                let count = bits.take(2) as usize + 2;
                let mut source = self
                    .quick_text_loc
                    .wrapping_sub(bits.take(8))
                    .wrapping_sub(1);
                ensure!(
                    out.len() + count <= output_len,
                    "quick DMS back-reference overruns output"
                );
                for _ in 0..count {
                    let byte = self.text[(source & 0x00FF) as usize];
                    out.push(byte);
                    self.text[(self.quick_text_loc & 0x00FF) as usize] = byte;
                    self.quick_text_loc = self.quick_text_loc.wrapping_add(1);
                    source = source.wrapping_add(1);
                }
            }
        }
        self.quick_text_loc = self.quick_text_loc.wrapping_add(5) & 0x00FF;
        Ok(out)
    }

    fn unpack_medium(&mut self, packed: &[u8], output_len: usize) -> Result<Vec<u8>> {
        let mut bits = BitReader::new(packed);
        let mut out = Vec::with_capacity(output_len);
        while out.len() < output_len {
            if bits.take(1) != 0 {
                let byte = bits.take(8) as u8;
                out.push(byte);
                self.text[(self.medium_text_loc & 0x3FFF) as usize] = byte;
                self.medium_text_loc = self.medium_text_loc.wrapping_add(1);
            } else {
                let mut code = bits.take(8) as usize;
                let count = D_CODE[code] as usize + 3;
                let len = D_LEN[code];
                code = (((code as u16) << len) | bits.take(len)) as usize & 0x00FF;
                let len = D_LEN[code];
                let offset = ((D_CODE[code] as u16) << 8)
                    | ((((code as u16) << len) | bits.take(len)) & 0x00FF);
                let mut source = self.medium_text_loc.wrapping_sub(offset).wrapping_sub(1);
                ensure!(
                    out.len() + count <= output_len,
                    "medium DMS back-reference overruns output"
                );
                for _ in 0..count {
                    let byte = self.text[(source & 0x3FFF) as usize];
                    out.push(byte);
                    self.text[(self.medium_text_loc & 0x3FFF) as usize] = byte;
                    self.medium_text_loc = self.medium_text_loc.wrapping_add(1);
                    source = source.wrapping_add(1);
                }
            }
        }
        self.medium_text_loc = self.medium_text_loc.wrapping_add(66) & 0x3FFF;
        Ok(out)
    }

    fn unpack_deep(&mut self, packed: &[u8], output_len: usize) -> Result<Vec<u8>> {
        let mut bits = BitReader::new(packed);
        if !self.deep.initialized {
            self.deep.init();
        }
        let mut out = Vec::with_capacity(output_len);
        while out.len() < output_len {
            let code = self.deep.decode_char(&mut bits);
            if code < 256 {
                let byte = code as u8;
                out.push(byte);
                self.text[(self.deep_text_loc & 0x3FFF) as usize] = byte;
                self.deep_text_loc = self.deep_text_loc.wrapping_add(1);
            } else {
                let count = code as usize - 253;
                let offset = self.deep.decode_position(&mut bits);
                let mut source = self.deep_text_loc.wrapping_sub(offset).wrapping_sub(1);
                ensure!(
                    out.len() + count <= output_len,
                    "deep DMS back-reference overruns output"
                );
                for _ in 0..count {
                    let byte = self.text[(source & 0x3FFF) as usize];
                    out.push(byte);
                    self.text[(self.deep_text_loc & 0x3FFF) as usize] = byte;
                    self.deep_text_loc = self.deep_text_loc.wrapping_add(1);
                    source = source.wrapping_add(1);
                }
            }
        }
        self.deep_text_loc = self.deep_text_loc.wrapping_add(60) & 0x3FFF;
        Ok(out)
    }

    fn unpack_heavy(&mut self, packed: &[u8], flags: u8, output_len: usize) -> Result<Vec<u8>> {
        const OFFSET: u16 = 253;
        let mut bits = BitReader::new(packed);
        let (np, bitmask) = if flags & 8 != 0 {
            (15usize, 0x1FFFu16)
        } else {
            (14usize, 0x0FFFu16)
        };
        self.heavy.np = np;
        if flags & 2 != 0 {
            self.heavy.read_tree_c(&mut bits)?;
            self.heavy.read_tree_p(&mut bits)?;
        }

        let mut out = Vec::with_capacity(output_len);
        while out.len() < output_len {
            let code = self.heavy.decode_c(&mut bits);
            if code < 256 {
                let byte = code as u8;
                out.push(byte);
                self.text[(self.heavy_text_loc & bitmask) as usize] = byte;
                self.heavy_text_loc = self.heavy_text_loc.wrapping_add(1);
            } else {
                let count = code as usize - OFFSET as usize;
                let offset = self.heavy.decode_p(&mut bits);
                let mut source = self.heavy_text_loc.wrapping_sub(offset).wrapping_sub(1);
                ensure!(
                    out.len() + count <= output_len,
                    "heavy DMS back-reference overruns output"
                );
                for _ in 0..count {
                    let byte = self.text[(source & bitmask) as usize];
                    out.push(byte);
                    self.text[(self.heavy_text_loc & bitmask) as usize] = byte;
                    self.heavy_text_loc = self.heavy_text_loc.wrapping_add(1);
                    source = source.wrapping_add(1);
                }
            }
        }
        Ok(out)
    }
}

fn unpack_rle(packed: &[u8], output_len: usize) -> Result<Vec<u8>> {
    let mut pos = 0usize;
    let mut out = Vec::with_capacity(output_len);
    while out.len() < output_len {
        let marker = read_byte(packed, &mut pos, "RLE marker")?;
        if marker != 0x90 {
            out.push(marker);
            continue;
        }

        let count_byte = read_byte(packed, &mut pos, "RLE count")?;
        if count_byte == 0 {
            out.push(0x90);
            continue;
        }

        let value = read_byte(packed, &mut pos, "RLE value")?;
        let count = if count_byte == 0xFF {
            let hi = read_byte(packed, &mut pos, "RLE extended count high")? as usize;
            let lo = read_byte(packed, &mut pos, "RLE extended count low")? as usize;
            (hi << 8) | lo
        } else {
            count_byte as usize
        };
        ensure!(
            out.len() + count <= output_len,
            "DMS RLE run overruns output"
        );
        out.resize(out.len() + count, value);
    }
    Ok(out)
}

#[derive(Clone)]
struct BitReader<'a> {
    data: &'a [u8],
    pos: usize,
    bitbuf: u32,
    bitcount: u8,
}

impl<'a> BitReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        let mut reader = Self {
            data,
            pos: 0,
            bitbuf: 0,
            bitcount: 0,
        };
        reader.drop(0);
        reader
    }

    fn take(&mut self, bits: u8) -> u16 {
        let value = self.get(bits);
        self.drop(bits);
        value
    }

    fn get(&self, bits: u8) -> u16 {
        if bits == 0 {
            return 0;
        }
        ((self.bitbuf >> (self.bitcount - bits)) & ((1u32 << bits) - 1)) as u16
    }

    fn drop(&mut self, bits: u8) {
        // `bits` is a fixed constant at most call sites, but a few pass a
        // Huffman code length read straight from an untrusted .dms stream
        // (see decode_c/decode_p); a corrupt length can exceed the bits
        // actually buffered. Saturate rather than underflow so a bogus
        // length degrades to a garbled decode (caught by the surrounding
        // CRC check) instead of a panic.
        self.bitcount = self.bitcount.saturating_sub(bits);
        self.bitbuf &= mask_bits(self.bitcount);
        while self.bitcount < 16 {
            let byte = self.data.get(self.pos).copied().unwrap_or(0);
            self.pos += 1;
            self.bitbuf = (self.bitbuf << 8) | byte as u32;
            self.bitcount += 8;
        }
    }
}

fn mask_bits(bits: u8) -> u32 {
    if bits == 0 {
        0
    } else {
        (1u32 << bits) - 1
    }
}

#[derive(Clone)]
struct HeavyState {
    left: [u16; 2 * HeavyState::NC - 1],
    right: [u16; 2 * HeavyState::NC - 1 + 9],
    c_len: [u8; HeavyState::NC],
    pt_len: [u8; HeavyState::NPT],
    c_table: [u16; 4096],
    pt_table: [u16; 256],
    last_len: u16,
    np: usize,
}

impl Default for HeavyState {
    fn default() -> Self {
        Self {
            left: [0; 2 * Self::NC - 1],
            right: [0; 2 * Self::NC - 1 + 9],
            c_len: [0; Self::NC],
            pt_len: [0; Self::NPT],
            c_table: [0; 4096],
            pt_table: [0; 256],
            last_len: 0,
            np: 14,
        }
    }
}

impl HeavyState {
    const NC: usize = 510;
    const NPT: usize = 20;
    const N1: u16 = 510;

    fn read_tree_c(&mut self, bits: &mut BitReader<'_>) -> Result<()> {
        let n = bits.take(9) as usize;
        if n > 0 {
            ensure!(n <= Self::NC, "DMS heavy C tree is too large");
            for len in self.c_len.iter_mut().take(n) {
                *len = bits.take(5) as u8;
            }
            self.c_len[n..].fill(0);
            make_table(
                Self::NC,
                &self.c_len,
                12,
                &mut self.c_table,
                &mut self.left,
                &mut self.right,
            )
            .context("building DMS heavy C table")?;
        } else {
            let code = bits.take(9);
            self.c_len.fill(0);
            self.c_table.fill(code);
        }
        Ok(())
    }

    fn read_tree_p(&mut self, bits: &mut BitReader<'_>) -> Result<()> {
        let n = bits.take(5) as usize;
        if n > 0 {
            ensure!(n <= self.np, "DMS heavy P tree is too large");
            for len in self.pt_len.iter_mut().take(n) {
                *len = bits.take(4) as u8;
            }
            self.pt_len[n..self.np].fill(0);
            make_table(
                self.np,
                &self.pt_len,
                8,
                &mut self.pt_table,
                &mut self.left,
                &mut self.right,
            )
            .context("building DMS heavy P table")?;
        } else {
            let code = bits.take(5);
            self.pt_len[..self.np].fill(0);
            self.pt_table.fill(code);
        }
        Ok(())
    }

    fn decode_c(&mut self, bits: &mut BitReader<'_>) -> u16 {
        let mut node = self.c_table[bits.get(12) as usize];
        if node < Self::N1 {
            bits.drop(self.c_len[node as usize]);
        } else {
            bits.drop(12);
            let branch_bits = bits.get(16);
            let mut mask = 0x8000u16;
            while node >= Self::N1 {
                node = if branch_bits & mask != 0 {
                    self.right[node as usize]
                } else {
                    self.left[node as usize]
                };
                mask >>= 1;
            }
            // `c_len[node]` is expected to exceed the 12-bit table prefix
            // for any leaf reached by the tree walk (that's why it wasn't
            // resolved directly by the table lookup above), but the length
            // table comes straight from an untrusted .dms stream: a
            // corrupt file that still passes make_table's Kraft-equality
            // check could still violate that. Saturate rather than
            // underflow; a bogus length here produces a garbled decode
            // that the surrounding CRC check catches, instead of a panic.
            bits.drop(self.c_len[node as usize].saturating_sub(12));
        }
        node
    }

    fn decode_p(&mut self, bits: &mut BitReader<'_>) -> u16 {
        let mut node = self.pt_table[bits.get(8) as usize];
        if node < self.np as u16 {
            bits.drop(self.pt_len[node as usize]);
        } else {
            bits.drop(8);
            let branch_bits = bits.get(16);
            let mut mask = 0x8000u16;
            while node >= self.np as u16 {
                node = if branch_bits & mask != 0 {
                    self.right[node as usize]
                } else {
                    self.left[node as usize]
                };
                mask >>= 1;
            }
            // Same reasoning as decode_c above: saturate instead of
            // underflowing on a length table that a corrupt stream could
            // still have smuggled past the Kraft-equality check.
            bits.drop(self.pt_len[node as usize].saturating_sub(8));
        }

        if node != self.np as u16 - 1 {
            if node > 0 {
                let extra_bits = (node - 1) as u8;
                node = bits.take(extra_bits) | (1u16 << (node - 1));
            }
            self.last_len = node;
        }
        self.last_len
    }
}

fn make_table(
    nchar: usize,
    bitlen: &[u8],
    tablebits: u8,
    table: &mut [u16],
    left: &mut [u16],
    right: &mut [u16],
) -> Result<()> {
    struct Maker<'a> {
        nchar: usize,
        bitlen: &'a [u8],
        table: &'a mut [u16],
        left: &'a mut [u16],
        right: &'a mut [u16],
        avail: usize,
        table_size: usize,
        len: u8,
        depth: u8,
        max_depth: u8,
        codeword: usize,
        bit: usize,
        c: isize,
    }

    impl Maker<'_> {
        fn build(&mut self) -> Result<()> {
            self.mktbl()?;
            self.mktbl()?;
            ensure!(
                self.codeword == self.table_size,
                "DMS heavy Huffman table did not consume the full code space"
            );
            Ok(())
        }

        fn mktbl(&mut self) -> Result<u16> {
            if self.len == self.depth {
                self.c += 1;
                while self.c < self.nchar as isize {
                    let idx = self.c as usize;
                    if self.bitlen[idx] == self.len {
                        let end = self.codeword + self.bit;
                        ensure!(
                            end <= self.table_size,
                            "DMS heavy Huffman code exceeds table size"
                        );
                        for slot in &mut self.table[self.codeword..end] {
                            *slot = idx as u16;
                        }
                        self.codeword = end;
                        return Ok(idx as u16);
                    }
                    self.c += 1;
                }
                self.c = -1;
                self.len += 1;
                self.bit >>= 1;
            }

            self.depth += 1;
            let found = if self.depth < self.max_depth {
                self.mktbl()?;
                self.mktbl()?;
                0
            } else if self.depth > 32 {
                bail!("DMS heavy Huffman tree is too deep");
            } else {
                let node = self.avail;
                ensure!(
                    node < 2 * self.nchar - 1,
                    "DMS heavy Huffman tree has too many nodes"
                );
                self.avail += 1;
                self.left[node] = self.mktbl()?;
                self.right[node] = self.mktbl()?;
                ensure!(
                    self.codeword < self.table_size,
                    "DMS heavy Huffman table overflow"
                );
                if self.depth == self.max_depth {
                    self.table[self.codeword] = node as u16;
                    self.codeword += 1;
                }
                node as u16
            };
            self.depth -= 1;
            Ok(found)
        }
    }

    ensure!(
        table.len() >= (1usize << tablebits),
        "DMS heavy table buffer is too small"
    );
    let mut maker = Maker {
        nchar,
        bitlen,
        table,
        left,
        right,
        avail: nchar,
        table_size: 1usize << tablebits,
        len: 1,
        depth: 1,
        max_depth: tablebits + 1,
        codeword: 0,
        bit: 1usize << (tablebits - 1),
        c: -1,
    };
    maker.build()
}

#[derive(Clone)]
struct DeepState {
    freq: [u16; DeepState::T + 1],
    parent: [u16; DeepState::T + DeepState::N_CHAR],
    son: [u16; DeepState::T],
    initialized: bool,
}

impl Default for DeepState {
    fn default() -> Self {
        Self {
            freq: [0; Self::T + 1],
            parent: [0; Self::T + Self::N_CHAR],
            son: [0; Self::T],
            initialized: false,
        }
    }
}

impl DeepState {
    const F: usize = 60;
    const THRESHOLD: usize = 2;
    const N_CHAR: usize = 256 - Self::THRESHOLD + Self::F;
    const T: usize = Self::N_CHAR * 2 - 1;
    const R: usize = Self::T - 1;
    const MAX_FREQ: u16 = 0x8000;

    fn init(&mut self) {
        for i in 0..Self::N_CHAR {
            self.freq[i] = 1;
            self.son[i] = (i + Self::T) as u16;
            self.parent[i + Self::T] = i as u16;
        }

        let mut i = 0usize;
        let mut j = Self::N_CHAR;
        while j <= Self::R {
            self.freq[j] = self.freq[i] + self.freq[i + 1];
            self.son[j] = i as u16;
            self.parent[i] = j as u16;
            self.parent[i + 1] = j as u16;
            i += 2;
            j += 1;
        }
        self.freq[Self::T] = 0xFFFF;
        self.parent[Self::R] = 0;
        self.initialized = true;
    }

    fn decode_char(&mut self, bits: &mut BitReader<'_>) -> u16 {
        let mut code = self.son[Self::R];
        while (code as usize) < Self::T {
            let bit = bits.take(1);
            code = self.son[code as usize + bit as usize];
        }
        code -= Self::T as u16;
        self.update(code);
        code
    }

    fn decode_position(&mut self, bits: &mut BitReader<'_>) -> u16 {
        let mut code = bits.take(8) as usize;
        let high = (D_CODE[code] as u16) << 8;
        let len = D_LEN[code];
        code = ((((code as u16) << len) | bits.take(len)) & 0x00FF) as usize;
        high | code as u16
    }

    fn reconst(&mut self) {
        let mut j = 0usize;
        for i in 0..Self::T {
            if self.son[i] as usize >= Self::T {
                self.freq[j] = self.freq[i].div_ceil(2);
                self.son[j] = self.son[i];
                j += 1;
            }
        }

        let mut i = 0usize;
        let mut j = Self::N_CHAR;
        while j < Self::T {
            let k = i + 1;
            let f = self.freq[i] + self.freq[k];
            self.freq[j] = f;
            let mut insert = j - 1;
            while f < self.freq[insert] {
                insert -= 1;
            }
            insert += 1;
            self.freq.copy_within(insert..j, insert + 1);
            self.freq[insert] = f;
            self.son.copy_within(insert..j, insert + 1);
            self.son[insert] = i as u16;
            i += 2;
            j += 1;
        }

        for i in 0..Self::T {
            let child = self.son[i] as usize;
            if child >= Self::T {
                self.parent[child] = i as u16;
            } else {
                self.parent[child] = i as u16;
                self.parent[child + 1] = i as u16;
            }
        }
    }

    fn update(&mut self, code: u16) {
        if self.freq[Self::R] == Self::MAX_FREQ {
            self.reconst();
        }

        let mut node = self.parent[code as usize + Self::T] as usize;
        loop {
            self.freq[node] += 1;
            let k = self.freq[node];
            let mut l = node + 1;
            if k > self.freq[l] {
                loop {
                    l += 1;
                    if k <= self.freq[l] {
                        break;
                    }
                }
                l -= 1;
                self.freq[node] = self.freq[l];
                self.freq[l] = k;

                let i = self.son[node] as usize;
                self.parent[i] = l as u16;
                if i < Self::T {
                    self.parent[i + 1] = l as u16;
                }

                let j = self.son[l] as usize;
                self.son[l] = i as u16;

                self.parent[j] = node as u16;
                if j < Self::T {
                    self.parent[j + 1] = node as u16;
                }
                self.son[node] = j as u16;

                node = l;
            }

            node = self.parent[node] as usize;
            if node == 0 {
                break;
            }
        }
    }
}

fn read_byte(data: &[u8], pos: &mut usize, what: &str) -> Result<u8> {
    let byte = data
        .get(*pos)
        .copied()
        .ok_or_else(|| anyhow!("DMS {what} is truncated"))?;
    *pos += 1;
    Ok(byte)
}

fn be_u16(data: &[u8]) -> u16 {
    u16::from_be_bytes([data[0], data[1]])
}

#[cfg(test)]
fn put_be_u16(data: &mut [u8], value: u16) {
    data.copy_from_slice(&value.to_be_bytes());
}

#[cfg(test)]
fn put_be_u24(data: &mut [u8], value: usize) {
    data[0] = ((value >> 16) & 0xFF) as u8;
    data[1] = ((value >> 8) & 0xFF) as u8;
    data[2] = (value & 0xFF) as u8;
}

fn checksum16(data: &[u8]) -> u16 {
    data.iter()
        .fold(0u16, |sum, byte| sum.wrapping_add(*byte as u16))
}

fn dms_crc16(data: &[u8]) -> u16 {
    let mut crc = 0u16;
    for &byte in data {
        crc ^= byte as u16;
        for _ in 0..8 {
            if crc & 1 != 0 {
                crc = (crc >> 1) ^ 0xA001;
            } else {
                crc >>= 1;
            }
        }
    }
    crc
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn no_compression_dms_decodes_to_adf() -> Result<()> {
        let mut adf = vec![0u8; ADF_SIZE];
        for (idx, byte) in adf.iter_mut().enumerate() {
            *byte = (idx as u8).wrapping_mul(17).wrapping_add(23);
        }
        let dms = make_uncompressed_dms(&adf);
        assert_eq!(decode_dms_adf(&dms)?, adf);
        Ok(())
    }

    #[test]
    fn decode_c_saturates_instead_of_underflowing_on_a_bogus_short_code_length() {
        // Force the table lookup to miss (so decode_c walks the tree) and
        // land on a leaf whose code length is shorter than the 12-bit
        // table prefix already consumed. A corrupt .dms Huffman length
        // table that still passes make_table's Kraft-equality check could
        // produce exactly this: `c_len[leaf] - 12` used to underflow.
        let mut heavy = HeavyState::default();
        heavy.c_table.fill(HeavyState::N1); // every 12-bit prefix misses
        heavy.left[HeavyState::N1 as usize] = 5; // ... and both tree
        heavy.right[HeavyState::N1 as usize] = 5; // branches land on leaf 5
        heavy.c_len[5] = 5; // shorter than the 12-bit prefix

        let data = [0u8; 8];
        let mut bits = BitReader::new(&data);
        let node = heavy.decode_c(&mut bits); // must not panic
        assert_eq!(node, 5);
    }

    #[test]
    fn state_of_the_art_sample_decodes_when_present() -> Result<()> {
        let path = Path::new("Spaceballs-StateOfTheArt.dms");
        if !path.exists() {
            return Ok(());
        }

        let dms = std::fs::read(path)?;
        let adf = decode_dms_adf(&dms)?;
        assert_eq!(adf.len(), ADF_SIZE);
        assert_eq!(&adf[..4], b"DOS\0");
        let byte_sum: u64 = adf.iter().map(|&b| b as u64).sum();
        assert_eq!(byte_sum, 90_696_112);
        Ok(())
    }

    fn make_uncompressed_dms(adf: &[u8]) -> Vec<u8> {
        assert_eq!(adf.len(), ADF_SIZE);
        let mut out = vec![0u8; HEAD_LEN];
        out[0..4].copy_from_slice(b"DMS!");
        put_be_u16(&mut out[16..18], 0);
        put_be_u16(&mut out[18..20], 79);
        put_be_u24(&mut out[21..24], ADF_SIZE);
        put_be_u24(&mut out[25..28], ADF_SIZE);
        put_be_u16(&mut out[46..48], 111);
        put_be_u16(&mut out[50..52], 0);
        put_be_u16(&mut out[52..54], 0);
        let hcrc = dms_crc16(&out[4..HEAD_LEN - 2]);
        put_be_u16(&mut out[HEAD_LEN - 2..HEAD_LEN], hcrc);

        for (track, chunk) in adf.chunks_exact(DMS_CYLINDER_BYTES).enumerate() {
            let mut header = [0u8; TRACK_HEAD_LEN];
            header[0..2].copy_from_slice(b"TR");
            put_be_u16(&mut header[2..4], track as u16);
            put_be_u16(&mut header[6..8], DMS_CYLINDER_BYTES as u16);
            put_be_u16(&mut header[8..10], DMS_CYLINDER_BYTES as u16);
            put_be_u16(&mut header[10..12], DMS_CYLINDER_BYTES as u16);
            header[13] = 0;
            put_be_u16(&mut header[14..16], checksum16(chunk));
            put_be_u16(&mut header[16..18], dms_crc16(chunk));
            let hcrc = dms_crc16(&header[..18]);
            put_be_u16(&mut header[18..20], hcrc);
            out.extend_from_slice(&header);
            out.extend_from_slice(chunk);
        }
        out
    }
}
