// SPDX-License-Identifier: GPL-3.0-or-later

//! The chunk layer of a `.clstate` file: tagged, individually versioned,
//! length-framed records whose payloads are self-describing MessagePack.
//!
//! [`ChunkWriter`] frames payloads, either whole when their length is known
//! or as a run of blocks ([`ChunkBody`]) when they are produced
//! incrementally. [`read_header`] and [`Body`] read them back straight from
//! the stream, so neither side ever holds more than one block of a payload
//! beyond what its consumer keeps: a machine with gigabytes of memory-backed
//! disk images saves and loads with the same memory footprint the flat
//! format had. Buffers grow only as far as the bytes actually present,
//! never as far as a claimed length. Unknown tags are skipped, so a build
//! can add a chunk without invalidating its states for older builds that
//! still understand the rest.
//!
//! Payload encoding ([`encode`]/[`decode`]): structs as maps keyed by field
//! name, enum variants by name, byte vectors as MessagePack `bin`, integers
//! at their natural width. Because every field is named, a serialized
//! struct may gain a field that carries `#[serde(default)]` (or is an
//! `Option`), lose a field, reorder its fields, or widen an integer without
//! any version change and old states still load. A chunk's version moves
//! only when a field's meaning or representation changes in a way a default
//! cannot express; a [`Migration`] step then rewrites the older shape, as a
//! value tree, before it is decoded into the live structs.

use std::fmt;
use std::io::{self, Cursor, Read, Take, Write};

use anyhow::{anyhow, bail, Context, Result};
use serde::de::DeserializeOwned;
use serde::Serialize;

/// A chunk's four-character tag.
pub type Tag = [u8; 4];

/// The end-of-stream marker: an empty version-0 chunk after the last real
/// one, so a stream cut short after a complete chunk is still detected.
pub const END: Tag = *b"END ";

/// In a chunk header, a payload length of all ones means the payload
/// follows as length-prefixed blocks rather than as that many bytes: the
/// writer streamed it without knowing the length up front.
pub(crate) const STREAMED: u64 = u64::MAX;

/// Block size for streamed payloads, and the most a payload buffer grows
/// per read.
const BLOCK: usize = 1 << 20;

/// What a chunk carries.
#[derive(Debug)]
pub(crate) enum Payload {
    /// One serialized value, written by `M68kMachine::write_chunks`.
    Value,
    /// A map of the named `Bus` fields, which must be contiguous in `Bus`
    /// declaration order so the chunk can stream.
    BusFields(&'static [&'static str]),
    /// A map of every `Bus` field no `BusFields` chunk claims.
    BusRest,
}

#[derive(Debug)]
pub(crate) struct ChunkSpec {
    pub tag: Tag,
    /// The version this build writes, and reads without migration.
    pub version: u32,
    /// What the chunk holds, for messages.
    pub name: &'static str,
    pub payload: Payload,
    /// A state without this chunk cannot rebuild a machine. Chunks that
    /// only hold `Option` (or `#[serde(default)]`) `Bus` fields are not
    /// required: their absence reads as the field's default.
    pub required: bool,
}

// Chunk version history. A chunk's version moves only for a change a
// serde default cannot express (see the module doc); additive changes
// need no entry here. Every bump gets a `Migration` below, or a note that
// none is possible.
//
//   all chunks v1: the chunked format (container version 81).

const fn value(tag: &[u8; 4], version: u32, name: &'static str) -> ChunkSpec {
    ChunkSpec {
        tag: *tag,
        version,
        name,
        payload: Payload::Value,
        required: true,
    }
}

const fn bus(
    tag: &[u8; 4],
    version: u32,
    name: &'static str,
    fields: &'static [&'static str],
    required: bool,
) -> ChunkSpec {
    ChunkSpec {
        tag: *tag,
        version,
        name,
        payload: Payload::BusFields(fields),
        required,
    }
}

/// The `MachineDescriptor` header chunk, uncompressed ahead of the rest.
pub(crate) const DESC: ChunkSpec = value(b"DESC", 1, "machine descriptor");
pub(crate) const CPU: ChunkSpec = value(b"CPU ", 1, "CPU core");
pub(crate) const MACH: ChunkSpec = value(b"MACH", 1, "machine runtime");
pub(crate) const ICAC: ChunkSpec = value(b"ICAC", 1, "instruction cache");
pub(crate) const DCAC: ChunkSpec = value(b"DCAC", 1, "data cache");
pub(crate) const MEM: ChunkSpec = bus(b"MEM ", 1, "memory", &["mem", "ram_init"], true);
pub(crate) const CIAA: ChunkSpec = bus(b"CIAA", 1, "CIA-A", &["cia_a"], true);
pub(crate) const CIAB: ChunkSpec = bus(b"CIAB", 1, "CIA-B", &["cia_b"], true);
pub(crate) const PAUL: ChunkSpec = bus(b"PAUL", 1, "Paula", &["paula"], true);
pub(crate) const AGNS: ChunkSpec = bus(b"AGNS", 1, "Agnus", &["agnus"], true);
pub(crate) const COPR: ChunkSpec = bus(b"COPR", 1, "Copper", &["copper"], true);
pub(crate) const DENI: ChunkSpec = bus(b"DENI", 1, "Denise", &["denise", "denise_revision"], true);
pub(crate) const BLIT: ChunkSpec = bus(b"BLIT", 1, "blitter", &["blitter"], true);
pub(crate) const FLOP: ChunkSpec = bus(b"FLOP", 1, "floppy controller", &["floppy"], true);
pub(crate) const RTC: ChunkSpec = bus(b"RTC ", 1, "real-time clock", &["rtc", "rtc_present"], true);
pub(crate) const GAYL: ChunkSpec = bus(b"GAYL", 1, "Gayle", &["gayle"], false);
pub(crate) const MOBO: ChunkSpec = bus(
    b"MOBO",
    1,
    "A3000/A4000 motherboard chips",
    &["ramsey", "gary", "sdmac", "ide_a4000"],
    false,
);
pub(crate) const UAEL: ChunkSpec = bus(b"UAEL", 1, "uaelib trap", &["uaelib"], false);
pub(crate) const CART: ChunkSpec = bus(b"CART", 1, "freezer cartridge", &["cartridge"], false);
pub(crate) const AKIK: ChunkSpec = bus(b"AKIK", 1, "Akiko", &["akiko"], false);
pub(crate) const CDTV: ChunkSpec = bus(b"CDTV", 1, "CDTV controller", &["cdtv"], false);
pub(crate) const ZORR: ChunkSpec = bus(b"ZORR", 1, "expansion boards", &["devices"], true);
pub(crate) const KEYB: ChunkSpec = bus(b"KEYB", 1, "keyboard", &["keyboard"], true);
pub(crate) const INPT: ChunkSpec = bus(b"INPT", 1, "controller ports", &["input"], true);
/// Everything else on the `Bus`: DMA arbitration, interrupt latches, beam
/// event capture, presentation windows, diagnostics. Written last, since
/// its fields are scattered through the struct.
pub(crate) const BUS: ChunkSpec = ChunkSpec {
    tag: *b"BUS ",
    version: 1,
    name: "bus glue",
    payload: Payload::BusRest,
    required: true,
};

/// Every chunk this build writes, in file order: the `Bus` chunks follow
/// the order of their fields in `Bus`, which is what lets them stream.
pub(crate) const CHUNKS: &[ChunkSpec] = &[
    DESC, CPU, MACH, ICAC, DCAC, MEM, CIAA, CIAB, PAUL, AGNS, COPR, DENI, BLIT, FLOP, RTC, GAYL,
    MOBO, UAEL, CART, AKIK, CDTV, ZORR, KEYB, INPT, BUS,
];

pub(crate) fn spec_for(tag: Tag) -> Option<&'static ChunkSpec> {
    CHUNKS.iter().find(|spec| spec.tag == tag)
}

/// The chunks that together make up the `Bus`, in file order.
pub(crate) fn bus_chunks() -> impl Iterator<Item = &'static ChunkSpec> {
    CHUNKS
        .iter()
        .filter(|spec| matches!(spec.payload, Payload::BusFields(_) | Payload::BusRest))
}

/// The chunk that carries a `Bus` field.
pub(crate) fn chunk_for_field(field: &str) -> &'static ChunkSpec {
    bus_chunks()
        .find(|spec| matches!(spec.payload, Payload::BusFields(fields) if fields.contains(&field)))
        .unwrap_or(&BUS)
}

/// A tag as text for messages: `BUS`, padding trimmed, or escaped bytes if
/// it is not printable ASCII.
pub(crate) fn tag_name(tag: Tag) -> String {
    format!("{}", tag.escape_ascii()).trim_end().to_string()
}

/// Identifies the state schema this build writes: the crate version, the
/// container version, and every chunk's tag and version. Two builds that
/// share it serialize a machine identically, which is what netplay checks
/// before trusting a peer's frame checksums.
pub const SCHEMA_FINGERPRINT: u32 = schema_fingerprint();

const fn fnv1a(mut hash: u32, bytes: &[u8]) -> u32 {
    let mut i = 0;
    while i < bytes.len() {
        hash ^= bytes[i] as u32;
        hash = hash.wrapping_mul(0x0100_0193);
        i += 1;
    }
    hash
}

const fn schema_fingerprint() -> u32 {
    let mut hash = fnv1a(0x811C_9DC5, env!("CARGO_PKG_VERSION").as_bytes());
    hash = fnv1a(hash, &super::STATE_VERSION.to_le_bytes());
    let mut i = 0;
    while i < CHUNKS.len() {
        hash = fnv1a(hash, &CHUNKS[i].tag);
        hash = fnv1a(hash, &CHUNKS[i].version.to_le_bytes());
        i += 1;
    }
    hash
}

/// The error type the split/join serde adapters report.
#[derive(Debug)]
pub(crate) struct CodecError(pub String);

impl fmt::Display for CodecError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for CodecError {}

impl serde::ser::Error for CodecError {
    fn custom<T: fmt::Display>(msg: T) -> Self {
        Self(msg.to_string())
    }
}

impl serde::de::Error for CodecError {
    fn custom<T: fmt::Display>(msg: T) -> Self {
        Self(msg.to_string())
    }
}

/// Serialize `value` in the state payload dialect.
pub(crate) fn encode<W: Write, T: Serialize + ?Sized>(
    value: &T,
    out: &mut W,
) -> std::result::Result<(), rmp_serde::encode::Error> {
    let mut serializer = rmp_serde::Serializer::new(out)
        .with_struct_map()
        .with_bytes(rmp_serde::config::BytesMode::ForceIterables);
    value.serialize(&mut serializer)
}

/// Nesting deeper than any state struct. The streaming decoder enforces it
/// itself; `check_shape` enforces it on payloads that are decoded from
/// memory, in particular by the value-tree decoder migrations use, which
/// has no limit of its own.
pub(crate) const MAX_NESTING: usize = 64;

/// Walk one MessagePack value without building it, checking that it is
/// well formed, ends exactly at the end of `bytes`, and nests no deeper
/// than `MAX_NESTING`, so a crafted payload of tens of thousands of nested
/// arrays fails the load instead of driving a recursive decoder off the
/// stack.
pub(crate) fn check_shape(bytes: &[u8]) -> Result<()> {
    use rmp::Marker;
    let mut cursor = bytes;
    let mut open: Vec<u64> = Vec::new();
    let mut pending: u64 = 1;
    loop {
        while pending == 0 {
            match open.pop() {
                Some(outer) => pending = outer,
                None => {
                    if !cursor.is_empty() {
                        bail!("{} bytes after the value", cursor.len());
                    }
                    return Ok(());
                }
            }
        }
        pending -= 1;
        let marker = rmp::decode::read_marker(&mut cursor)
            .map_err(|e| anyhow!("malformed value: {}", e.0))?;
        let len_of = |cursor: &mut &[u8], width: usize| -> Result<u64> {
            let mut buf = [0u8; 4];
            let (head, rest) = cursor
                .split_at_checked(width)
                .ok_or_else(|| anyhow!("truncated length prefix"))?;
            buf[4 - width..].copy_from_slice(head);
            *cursor = rest;
            Ok(u64::from(u32::from_be_bytes(buf)))
        };
        let (skip, children) = match marker {
            Marker::FixPos(_) | Marker::FixNeg(_) | Marker::Null | Marker::True | Marker::False => {
                (0, 0)
            }
            Marker::U8 | Marker::I8 => (1, 0),
            Marker::U16 | Marker::I16 => (2, 0),
            Marker::U32 | Marker::I32 | Marker::F32 => (4, 0),
            Marker::U64 | Marker::I64 | Marker::F64 => (8, 0),
            Marker::FixStr(n) => (u64::from(n), 0),
            Marker::Str8 | Marker::Bin8 => (len_of(&mut cursor, 1)?, 0),
            Marker::Str16 | Marker::Bin16 => (len_of(&mut cursor, 2)?, 0),
            Marker::Str32 | Marker::Bin32 => (len_of(&mut cursor, 4)?, 0),
            Marker::FixExt1 => (2, 0),
            Marker::FixExt2 => (3, 0),
            Marker::FixExt4 => (5, 0),
            Marker::FixExt8 => (9, 0),
            Marker::FixExt16 => (17, 0),
            Marker::Ext8 => (len_of(&mut cursor, 1)? + 1, 0),
            Marker::Ext16 => (len_of(&mut cursor, 2)? + 1, 0),
            Marker::Ext32 => (len_of(&mut cursor, 4)? + 1, 0),
            Marker::FixArray(n) => (0, u64::from(n)),
            Marker::Array16 => (0, len_of(&mut cursor, 2)?),
            Marker::Array32 => (0, len_of(&mut cursor, 4)?),
            Marker::FixMap(n) => (0, 2 * u64::from(n)),
            Marker::Map16 => (0, 2 * len_of(&mut cursor, 2)?),
            Marker::Map32 => (0, 2 * len_of(&mut cursor, 4)?),
            Marker::Reserved => bail!("reserved MessagePack marker"),
        };
        if skip > 0 {
            let skip =
                usize::try_from(skip).map_err(|_| anyhow!("value longer than the payload"))?;
            cursor = cursor
                .get(skip..)
                .ok_or_else(|| anyhow!("value longer than the payload"))?;
        }
        if children > 0 {
            open.push(pending);
            pending = children;
            if open.len() > MAX_NESTING {
                bail!("nested deeper than {MAX_NESTING} levels");
            }
        }
    }
}

/// Decode one value from a whole payload held in memory, which must hold
/// exactly that value (`check_shape` refuses anything else first).
pub(crate) fn decode<T: DeserializeOwned>(bytes: &[u8]) -> Result<T> {
    check_shape(bytes)?;
    let mut deserializer = rmp_serde::Deserializer::from_read_ref(bytes);
    T::deserialize(&mut deserializer).map_err(|e| anyhow!("{e}"))
}

/// Frames chunks onto a byte sink.
#[derive(Debug)]
pub(crate) struct ChunkWriter<W: Write> {
    inner: W,
}

impl<W: Write> ChunkWriter<W> {
    pub fn new(inner: W) -> Self {
        Self { inner }
    }

    fn header(&mut self, tag: Tag, version: u32, len: u64) -> io::Result<()> {
        self.inner.write_all(&tag)?;
        self.inner.write_all(&version.to_le_bytes())?;
        self.inner.write_all(&len.to_le_bytes())
    }

    /// Frame an already encoded `payload` as `spec`'s chunk at its current
    /// version.
    pub fn write(&mut self, spec: &ChunkSpec, payload: &[u8]) -> Result<()> {
        self.write_raw(spec.tag, spec.version, payload)
            .with_context(|| format!("writing {} chunk ({})", tag_name(spec.tag), spec.name))
    }

    pub fn write_raw(&mut self, tag: Tag, version: u32, payload: &[u8]) -> Result<()> {
        self.header(tag, version, payload.len() as u64)?;
        self.inner.write_all(payload)?;
        Ok(())
    }

    /// Encode and frame one value.
    pub fn value<T: Serialize + ?Sized>(&mut self, spec: &ChunkSpec, value: &T) -> Result<()> {
        let mut payload = Vec::new();
        encode(value, &mut payload).map_err(|e| anyhow!("serializing {}: {e}", spec.name))?;
        self.write(spec, &payload)
    }

    /// Open `spec`'s chunk for a payload written incrementally as blocks.
    /// The body hands the writer back on `ChunkBody::finish`.
    pub fn stream(mut self, spec: &ChunkSpec) -> Result<ChunkBody<W>> {
        self.header(spec.tag, spec.version, STREAMED)
            .with_context(|| format!("writing {} chunk ({})", tag_name(spec.tag), spec.name))?;
        Ok(ChunkBody {
            writer: self,
            buf: Vec::with_capacity(BLOCK),
        })
    }

    /// Write the end marker and hand the sink back.
    pub fn finish(mut self) -> Result<W> {
        self.header(END, 0, 0)?;
        Ok(self.inner)
    }
}

/// A streamed chunk payload: collects up to a block's worth of bytes, then
/// writes them as one length-prefixed block, and a zero-length block on
/// `finish`. A payload of any size therefore costs one block of memory.
pub(crate) struct ChunkBody<W: Write> {
    writer: ChunkWriter<W>,
    buf: Vec<u8>,
}

impl<W: Write> ChunkBody<W> {
    fn flush_block(&mut self) -> io::Result<()> {
        if !self.buf.is_empty() {
            let len = u32::try_from(self.buf.len()).expect("blocks are at most BLOCK bytes");
            self.writer.inner.write_all(&len.to_le_bytes())?;
            self.writer.inner.write_all(&self.buf)?;
            self.buf.clear();
        }
        Ok(())
    }

    /// Close the payload and hand the chunk writer back.
    pub fn finish(mut self) -> Result<ChunkWriter<W>> {
        self.flush_block()?;
        self.writer.inner.write_all(&0u32.to_le_bytes())?;
        Ok(self.writer)
    }
}

impl<W: Write> Write for ChunkBody<W> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let room = BLOCK - self.buf.len();
        let take = bytes.len().min(room);
        self.buf.extend_from_slice(&bytes[..take]);
        if self.buf.len() >= BLOCK {
            self.flush_block()?;
        }
        Ok(take)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// One chunk's header as read from a stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ChunkHeader {
    pub tag: Tag,
    pub version: u32,
    /// Payload length, or `STREAMED`.
    pub len: u64,
}

pub(crate) fn read_header<R: Read>(reader: &mut R) -> Result<ChunkHeader> {
    let mut header = [0u8; 16];
    reader
        .read_exact(&mut header)
        .context("reading chunk header")?;
    Ok(ChunkHeader {
        tag: header[0..4].try_into().expect("four tag bytes"),
        version: u32::from_le_bytes(header[4..8].try_into().expect("four version bytes")),
        len: u64::from_le_bytes(header[8..16].try_into().expect("eight length bytes")),
    })
}

/// Check the end marker and that nothing follows it. Reading on past the
/// marker also drives the zlib decoder through the end of its stream, so
/// a corrupt trailer checksum surfaces here. (A stream cut off after the
/// marker is not an error: every chunk before it arrived complete.)
pub(crate) fn finish_stream<R: Read>(end: &ChunkHeader, reader: &mut R) -> Result<()> {
    if end.version != 0 || end.len != 0 {
        bail!(
            "malformed END marker (version {}, length {})",
            end.version,
            end.len
        );
    }
    let mut probe = [0u8; 1];
    match reader.read(&mut probe) {
        Ok(0) => Ok(()),
        Ok(_) => bail!("data after the END marker"),
        Err(e) => Err(e).context("verifying the end of the compressed body"),
    }
}

/// A run of length-prefixed blocks, ended by a zero-length block.
pub(crate) struct BlockReader<R: Read> {
    inner: R,
    remaining: u32,
    done: bool,
}

impl<R: Read> BlockReader<R> {
    /// Hand the stream back once the last block has been read.
    fn finish(mut self) -> Result<R> {
        if !self.done {
            if self.remaining != 0 {
                bail!("{} unread bytes in the payload", self.remaining);
            }
            let next = self.next_block()?;
            if next != 0 {
                bail!("an unread payload block of {next} bytes");
            }
        }
        Ok(self.inner)
    }

    fn next_block(&mut self) -> io::Result<u32> {
        let mut len = [0u8; 4];
        self.inner.read_exact(&mut len)?;
        let len = u32::from_le_bytes(len);
        if len == 0 {
            self.done = true;
        }
        self.remaining = len;
        Ok(len)
    }
}

impl<R: Read> Read for BlockReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.done || buf.is_empty() {
            return Ok(0);
        }
        if self.remaining == 0 && self.next_block()? == 0 {
            return Ok(0);
        }
        let want = buf.len().min(self.remaining as usize);
        let n = self.inner.read(&mut buf[..want])?;
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "payload block cut short",
            ));
        }
        self.remaining -= n as u32;
        Ok(n)
    }
}

/// One chunk's payload, read straight from the stream: either the `len`
/// bytes its header named, or a run of blocks.
pub(crate) enum Body<R: Read> {
    Plain(Take<R>),
    Blocks(BlockReader<R>),
}

impl<R: Read> Body<R> {
    pub fn open(header: &ChunkHeader, reader: R) -> Self {
        if header.len == STREAMED {
            Self::Blocks(BlockReader {
                inner: reader,
                remaining: 0,
                done: false,
            })
        } else {
            Self::Plain(reader.take(header.len))
        }
    }

    /// Hand the stream back; the payload must have been consumed exactly.
    pub fn finish(self) -> Result<R> {
        match self {
            Self::Plain(take) => {
                if take.limit() != 0 {
                    bail!("{} unread bytes in the payload", take.limit());
                }
                Ok(take.into_inner())
            }
            Self::Blocks(blocks) => blocks.finish(),
        }
    }

    /// Drain the payload, returning its byte count and the stream.
    pub fn skip(mut self) -> Result<(u64, R)> {
        let count = io::copy(&mut self, &mut io::sink()).context("skipping chunk payload")?;
        self.check_complete()?;
        Ok((count, self.finish()?))
    }

    /// Read the whole payload, growing the buffer one block at a time so
    /// a header claiming more than the stream holds fails at the end of
    /// the stream having allocated no more than one block past it.
    pub fn read_to_vec(mut self) -> Result<(Vec<u8>, R)> {
        let mut out = Vec::new();
        loop {
            let start = out.len();
            out.resize(start + BLOCK, 0);
            let mut filled = 0;
            while filled < BLOCK {
                match self.read(&mut out[start + filled..]) {
                    Ok(0) => break,
                    Ok(n) => filled += n,
                    Err(e) => return Err(e).context("reading chunk payload"),
                }
            }
            out.truncate(start + filled);
            if filled < BLOCK {
                break;
            }
        }
        self.check_complete()?;
        Ok((out, self.finish()?))
    }

    /// After a read to end of payload: a plain payload the stream ran out
    /// of before `len` bytes is truncation, not unread data.
    fn check_complete(&self) -> Result<()> {
        if let Self::Plain(take) = self {
            if take.limit() != 0 {
                bail!(
                    "chunk payload cut short: {} bytes claimed but missing",
                    take.limit()
                );
            }
        }
        Ok(())
    }
}

impl<R: Read> Read for Body<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Self::Plain(take) => take.read(buf),
            Self::Blocks(blocks) => blocks.read(buf),
        }
    }
}

/// Where a streamed `Bus` chunk's field map is being decoded from.
pub(crate) enum PartSource<R: Read> {
    /// Straight from the stream.
    Stream(Body<R>),
    /// A payload rewritten by a migration, held in memory.
    Owned(Cursor<Vec<u8>>),
}

impl<R: Read> Read for PartSource<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Self::Stream(body) => body.read(buf),
            Self::Owned(cursor) => cursor.read(buf),
        }
    }
}

/// Receives the non-`Bus` chunks a `BusJoiner` meets while feeding the
/// `Bus` visitor, already upgraded to the version this build reads.
pub(crate) trait ValueSink {
    fn value(&mut self, spec: &'static ChunkSpec, payload: Vec<u8>) -> Result<()>;
}

/// One upgrade step for a chunk: rewrites a payload written at `from` into
/// the shape of `from + 1`, on the decoded value tree.
pub(crate) struct Migration {
    pub tag: Tag,
    pub from: u32,
    pub apply: fn(&mut rmpv::Value) -> Result<()>,
}

/// Every upgrade step this build knows. When a chunk's version moves, add
/// the step here that rewrites the previous shape (rename or restructure
/// fields, fill values a default cannot express) so states written at the
/// previous version keep loading.
pub(crate) const MIGRATIONS: &[Migration] = &[];

/// Bring a chunk's payload to `spec.version`: returned as is when the
/// versions match, rewritten through the migration steps when older.
pub(crate) fn upgrade(
    spec: &ChunkSpec,
    version: u32,
    payload: Vec<u8>,
    migrations: &[Migration],
) -> Result<Vec<u8>> {
    let name = tag_name(spec.tag);
    if version == spec.version {
        return Ok(payload);
    }
    if version > spec.version {
        bail!(
            "{name} chunk ({}) is version {version}, newer than the version {} this build reads; \
             the state comes from a newer Copperline",
            spec.name,
            spec.version
        );
    }
    check_shape(&payload).with_context(|| format!("{name} chunk ({})", spec.name))?;
    let mut value = rmpv::decode::read_value(&mut &payload[..])
        .map_err(|e| anyhow!("{name} chunk ({}): {e}", spec.name))?;
    let mut at = version;
    while at < spec.version {
        let step = migrations
            .iter()
            .find(|m| m.tag == spec.tag && m.from == at)
            .ok_or_else(|| {
                anyhow!(
                    "{name} chunk ({}) is version {version}; this build reads version {} and has \
                     no upgrade from version {at}",
                    spec.name,
                    spec.version
                )
            })?;
        (step.apply)(&mut value)
            .with_context(|| format!("upgrading {name} chunk from version {at}"))?;
        at += 1;
    }
    let mut out = Vec::new();
    rmpv::encode::write_value(&mut out, &value)
        .map_err(|e| anyhow!("re-encoding upgraded {name} chunk: {e}"))?;
    Ok(out)
}
