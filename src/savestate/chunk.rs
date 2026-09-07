// SPDX-License-Identifier: GPL-3.0-or-later

//! The chunk layer of a `.clstate` file: tagged, individually versioned,
//! length-framed records whose payloads are self-describing MessagePack.
//!
//! [`ChunkWriter`] frames payloads; [`ChunkSet`] reads a whole stream back,
//! growing its buffers only as far as the bytes actually present (never as
//! far as a claimed length), and hands each known chunk to its decoder once
//! any [`Migration`] steps have brought it up to the version this build
//! reads. Unknown tags are skipped, so a build can add a chunk without
//! invalidating its states for older builds that still understand the rest.
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

use std::borrow::Cow;
use std::fmt;
use std::io::{Read, Write};

use anyhow::{anyhow, bail, Context, Result};
use serde::de::DeserializeOwned;
use serde::Serialize;

/// A chunk's four-character tag.
pub type Tag = [u8; 4];

/// The end-of-stream marker: a zero-length chunk after the last real one, so
/// a stream cut short after a complete chunk is still detected.
pub const END: Tag = *b"END ";

/// What a chunk carries.
#[derive(Debug)]
pub(crate) enum Payload {
    /// One serialized value, written by `M68kMachine::write_chunks`.
    Value,
    /// A map of the named `Bus` fields.
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
pub(crate) const KEYB: ChunkSpec = bus(b"KEYB", 1, "keyboard", &["keyboard"], true);
pub(crate) const INPT: ChunkSpec = bus(b"INPT", 1, "controller ports", &["input"], true);
pub(crate) const GAYL: ChunkSpec = bus(b"GAYL", 1, "Gayle", &["gayle"], false);
pub(crate) const MOBO: ChunkSpec = bus(
    b"MOBO",
    1,
    "A3000/A4000 motherboard chips",
    &["ramsey", "gary", "sdmac", "ide_a4000"],
    false,
);
pub(crate) const AKIK: ChunkSpec = bus(b"AKIK", 1, "Akiko", &["akiko"], false);
pub(crate) const CDTV: ChunkSpec = bus(b"CDTV", 1, "CDTV controller", &["cdtv"], false);
pub(crate) const ZORR: ChunkSpec = bus(b"ZORR", 1, "expansion boards", &["devices"], true);
pub(crate) const CART: ChunkSpec = bus(b"CART", 1, "freezer cartridge", &["cartridge"], false);
pub(crate) const UAEL: ChunkSpec = bus(b"UAEL", 1, "uaelib trap", &["uaelib"], false);
/// Everything else on the `Bus`: DMA arbitration, interrupt latches, beam
/// event capture, presentation windows, diagnostics.
pub(crate) const BUS: ChunkSpec = ChunkSpec {
    tag: *b"BUS ",
    version: 1,
    name: "bus glue",
    payload: Payload::BusRest,
    required: true,
};

/// Every chunk this build writes, in file order.
pub(crate) const CHUNKS: &[ChunkSpec] = &[
    DESC, CPU, MACH, ICAC, DCAC, MEM, CIAA, CIAB, PAUL, AGNS, COPR, DENI, BLIT, FLOP, RTC, KEYB,
    INPT, GAYL, MOBO, AKIK, CDTV, ZORR, CART, UAEL, BUS,
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
pub(crate) fn encode<T: Serialize + ?Sized>(
    value: &T,
    out: &mut Vec<u8>,
) -> std::result::Result<(), rmp_serde::encode::Error> {
    let mut serializer = rmp_serde::Serializer::new(out)
        .with_struct_map()
        .with_bytes(rmp_serde::config::BytesMode::ForceIterables);
    value.serialize(&mut serializer)
}

/// Nesting deeper than any state struct: the walker refuses payloads past
/// it before the recursive decoders see them.
const MAX_NESTING: usize = 64;

/// Walk one MessagePack value without building it, checking that it is
/// well formed, ends exactly at the end of `bytes`, and nests no deeper
/// than `MAX_NESTING`. The value-tree decoder that migrations use recurses
/// per nesting level with no limit of its own, so a crafted payload of tens
/// of thousands of nested arrays would otherwise overflow the stack instead
/// of failing the load; and a payload known to hold exactly one value lets
/// the decoders skip their own trailing-byte bookkeeping.
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

/// Decode one value from a whole payload, which must hold exactly that
/// value (`check_shape` refuses anything else before the decoder runs).
pub(crate) fn decode<T: DeserializeOwned>(bytes: &[u8]) -> Result<T> {
    check_shape(bytes)?;
    let mut deserializer = rmp_serde::Deserializer::from_read_ref(bytes);
    T::deserialize(&mut deserializer).map_err(|e| anyhow!("{e}"))
}

/// Frames chunks onto a byte sink.
pub(crate) struct ChunkWriter<W: Write> {
    inner: W,
}

impl<W: Write> ChunkWriter<W> {
    pub fn new(inner: W) -> Self {
        Self { inner }
    }

    /// Frame an already encoded `payload` as `spec`'s chunk at its current
    /// version.
    pub fn write(&mut self, spec: &ChunkSpec, payload: &[u8]) -> Result<()> {
        self.write_raw(spec.tag, spec.version, payload)
            .with_context(|| format!("writing {} chunk ({})", tag_name(spec.tag), spec.name))
    }

    pub fn write_raw(&mut self, tag: Tag, version: u32, payload: &[u8]) -> Result<()> {
        self.inner.write_all(&tag)?;
        self.inner.write_all(&version.to_le_bytes())?;
        self.inner
            .write_all(&(payload.len() as u64).to_le_bytes())?;
        self.inner.write_all(payload)?;
        Ok(())
    }

    /// Encode and frame one value.
    pub fn value<T: Serialize + ?Sized>(&mut self, spec: &ChunkSpec, value: &T) -> Result<()> {
        let mut payload = Vec::new();
        encode(value, &mut payload).map_err(|e| anyhow!("serializing {}: {e}", spec.name))?;
        self.write(spec, &payload)
    }

    /// Write the end marker and hand the sink back.
    pub fn finish(mut self) -> Result<W> {
        self.write_raw(END, 0, &[])?;
        Ok(self.inner)
    }
}

/// One chunk as read from a stream.
#[derive(Debug)]
pub(crate) struct Chunk {
    pub tag: Tag,
    pub version: u32,
    pub payload: Vec<u8>,
}

/// How much a payload buffer grows per read: the most a state can
/// over-allocate past the bytes it actually holds.
const FILL_STEP: u64 = 1 << 20;

/// Read one chunk. The payload buffer grows a step at a time, so a header
/// claiming more bytes than the stream holds fails at the end of the
/// stream having allocated no more than one step beyond what exists.
pub(crate) fn read_chunk<R: Read>(reader: &mut R) -> Result<Chunk> {
    let mut header = [0u8; 16];
    reader
        .read_exact(&mut header)
        .context("reading chunk header")?;
    let tag: Tag = header[0..4].try_into().expect("four tag bytes");
    let version = u32::from_le_bytes(header[4..8].try_into().expect("four version bytes"));
    let len = u64::from_le_bytes(header[8..16].try_into().expect("eight length bytes"));
    let mut payload = Vec::new();
    let mut remaining = len;
    while remaining > 0 {
        let step = remaining.min(FILL_STEP) as usize;
        let start = payload.len();
        payload.resize(start + step, 0);
        reader.read_exact(&mut payload[start..]).with_context(|| {
            format!(
                "reading {} chunk payload ({len} bytes claimed)",
                tag_name(tag)
            )
        })?;
        remaining -= step as u64;
    }
    Ok(Chunk {
        tag,
        version,
        payload,
    })
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

/// Bring a chunk's payload to `spec.version`: borrowed as written when the
/// versions match, rewritten through the migration steps when older.
pub(crate) fn upgrade<'a>(
    spec: &ChunkSpec,
    chunk: &'a Chunk,
    migrations: &[Migration],
) -> Result<Cow<'a, [u8]>> {
    let name = tag_name(spec.tag);
    if chunk.version == spec.version {
        return Ok(Cow::Borrowed(&chunk.payload));
    }
    if chunk.version > spec.version {
        bail!(
            "{name} chunk ({}) is version {}, newer than the version {} this build reads; \
             the state comes from a newer Copperline",
            spec.name,
            chunk.version,
            spec.version
        );
    }
    check_shape(&chunk.payload).with_context(|| format!("{name} chunk ({})", spec.name))?;
    let mut value = rmpv::decode::read_value(&mut &chunk.payload[..])
        .map_err(|e| anyhow!("{name} chunk ({}): {e}", spec.name))?;
    let mut version = chunk.version;
    while version < spec.version {
        let step = migrations
            .iter()
            .find(|m| m.tag == spec.tag && m.from == version)
            .ok_or_else(|| {
                anyhow!(
                    "{name} chunk ({}) is version {}; this build reads version {} and has \
                     no upgrade from version {version}",
                    spec.name,
                    chunk.version,
                    spec.version
                )
            })?;
        (step.apply)(&mut value)
            .with_context(|| format!("upgrading {name} chunk from version {version}"))?;
        version += 1;
    }
    let mut out = Vec::new();
    rmpv::encode::write_value(&mut out, &value)
        .map_err(|e| anyhow!("re-encoding upgraded {name} chunk: {e}"))?;
    Ok(Cow::Owned(out))
}

/// One `Bus` chunk's spec and its payload at the version this build reads.
pub(crate) type BusPart<'a> = (&'static ChunkSpec, Cow<'a, [u8]>);

/// The chunks of one state, read up to the end marker.
pub(crate) struct ChunkSet {
    chunks: Vec<Chunk>,
}

impl ChunkSet {
    /// Read chunks until the end marker. Unknown tags are dropped with a
    /// warning; a tag seen twice is an error.
    pub fn read<R: Read>(reader: &mut R) -> Result<Self> {
        let mut chunks: Vec<Chunk> = Vec::new();
        loop {
            let chunk = read_chunk(reader)?;
            if chunk.tag == END {
                break;
            }
            if chunks.iter().any(|seen| seen.tag == chunk.tag) {
                bail!("duplicate {} chunk", tag_name(chunk.tag));
            }
            if spec_for(chunk.tag).is_none() {
                log::warn!(
                    "save state: skipping unknown {} chunk (version {}, {} bytes)",
                    tag_name(chunk.tag),
                    chunk.version,
                    chunk.payload.len()
                );
                continue;
            }
            chunks.push(chunk);
        }
        Ok(Self { chunks })
    }

    pub fn get(&self, spec: &ChunkSpec) -> Option<&Chunk> {
        self.chunks.iter().find(|chunk| chunk.tag == spec.tag)
    }

    /// `spec`'s payload at the version this build reads, if the chunk is
    /// present.
    pub fn payload(
        &self,
        spec: &ChunkSpec,
        migrations: &[Migration],
    ) -> Result<Option<Cow<'_, [u8]>>> {
        self.get(spec)
            .map(|chunk| upgrade(spec, chunk, migrations))
            .transpose()
    }

    /// Decode a `Payload::Value` chunk.
    pub fn value<T: DeserializeOwned>(
        &self,
        spec: &ChunkSpec,
        migrations: &[Migration],
    ) -> Result<T> {
        let payload = self
            .payload(spec, migrations)?
            .ok_or_else(|| anyhow!("state has no {} chunk ({})", tag_name(spec.tag), spec.name))?;
        decode(&payload)
            .with_context(|| format!("reading {} chunk ({})", tag_name(spec.tag), spec.name))
    }

    /// Every `Bus` chunk present, upgraded, in file order; a required one
    /// missing is an error.
    pub fn bus_parts(&self, migrations: &[Migration]) -> Result<Vec<BusPart<'_>>> {
        let mut parts = Vec::new();
        for spec in bus_chunks() {
            match self.payload(spec, migrations)? {
                Some(payload) => parts.push((spec, payload)),
                None if spec.required => {
                    bail!("state has no {} chunk ({})", tag_name(spec.tag), spec.name)
                }
                None => {}
            }
        }
        Ok(parts)
    }
}
