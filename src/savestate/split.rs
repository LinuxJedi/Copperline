// SPDX-License-Identifier: GPL-3.0-or-later

//! Splitting the `Bus` into subsystem chunks, and joining them back.
//!
//! `Bus` keeps its derived serde impls. [`BusSplitter`] is a `Serializer`
//! that accepts only a struct and streams each field into the chunk that
//! claims it (`ChunkSpec::payload`): a chunk opens when its first field
//! arrives and closes when a field of another chunk does, so the chunks
//! follow the order of the fields in `Bus` and only the catch-all `BUS `
//! chunk, whose fields are scattered through the struct, is buffered until
//! the end. A field added to `Bus` lands there with no table edit.
//!
//! [`BusJoiner`] is the `Deserializer` counterpart: it pulls chunks from
//! the stream as the derived visitor asks for the next field, decoding
//! each `Bus` chunk's field map straight from the stream and handing the
//! others to a [`ValueSink`], so `Bus::deserialize` sees exactly what a
//! single-map encoding would have given it (any order, unknown fields
//! ignored, missing ones defaulted where the struct allows) while no more
//! than one block of payload is buffered at a time.

use std::io::{Cursor, Read, Write};

use serde::de::value::StrDeserializer;
use serde::de::{DeserializeSeed, MapAccess, Visitor};
use serde::ser::{Impossible, Serialize, SerializeStruct};

use super::chunk::{
    self, finish_stream, read_header, tag_name, Body, ChunkBody, ChunkSpec, ChunkWriter,
    CodecError, Migration, PartSource, Payload, Tag, ValueSink, MAX_NESTING,
};

/// Where the splitter's output currently goes.
enum Sink<W: Write> {
    Idle(ChunkWriter<W>),
    Streaming {
        spec: &'static ChunkSpec,
        body: ChunkBody<W>,
        expected: u32,
        written: u32,
    },
}

/// Serializes one struct's fields into per-chunk field maps, streaming
/// each chunk as its fields arrive.
pub(crate) struct BusSplitter<W: Write> {
    sink: Option<Sink<W>>,
    /// The catch-all chunk's entries, written last.
    rest: Vec<u8>,
    rest_written: u32,
}

impl<W: Write> BusSplitter<W> {
    /// Serialize `value` (a struct) into one chunk per `Bus` chunk spec and
    /// hand the writer back for the caller to finish.
    pub fn split<T: Serialize + ?Sized>(
        value: &T,
        writer: ChunkWriter<W>,
    ) -> Result<ChunkWriter<W>, CodecError> {
        let mut splitter = Self {
            sink: Some(Sink::Idle(writer)),
            rest: Vec::new(),
            rest_written: 0,
        };
        value.serialize(&mut splitter)?;
        match splitter.sink.take() {
            Some(Sink::Idle(writer)) => Ok(writer),
            _ => Err(CodecError("the bus serialization did not finish".into())),
        }
    }

    fn io(e: impl std::fmt::Display) -> CodecError {
        CodecError(e.to_string())
    }

    /// Close the streaming chunk, if any, leaving the writer idle.
    fn close(&mut self) -> Result<(), CodecError> {
        if let Some(Sink::Streaming {
            spec,
            body,
            expected,
            written,
        }) = self
            .sink
            .take_if(|sink| matches!(sink, Sink::Streaming { .. }))
        {
            // A chunk closes when a field of another chunk arrives, so a
            // short count means a listed field is missing from the struct
            // or not adjacent to the chunk's other fields.
            if written != expected {
                return Err(CodecError(format!(
                    "{} chunk claims {expected} bus fields but {written} were serialized \
                     together; check its field list in savestate/chunk.rs and that those \
                     fields are adjacent in Bus",
                    tag_name(spec.tag)
                )));
            }
            self.sink = Some(Sink::Idle(body.finish().map_err(Self::io)?));
        }
        Ok(())
    }

    /// Make `spec` the streaming chunk, closing the previous one.
    fn open(&mut self, spec: &'static ChunkSpec, expected: u32) -> Result<(), CodecError> {
        if let Some(Sink::Streaming { spec: current, .. }) = &self.sink {
            if current.tag == spec.tag {
                return Ok(());
            }
        }
        self.close()?;
        let Some(Sink::Idle(writer)) = self.sink.take() else {
            return Err(CodecError("no chunk writer".into()));
        };
        let mut body = writer.stream(spec).map_err(Self::io)?;
        rmp::encode::write_map_len(&mut body, expected).map_err(Self::io)?;
        self.sink = Some(Sink::Streaming {
            spec,
            body,
            expected,
            written: 0,
        });
        Ok(())
    }
}

fn not_a_struct() -> CodecError {
    CodecError("the bus serializes as a struct".to_string())
}

macro_rules! refuse_scalars {
    ($($method:ident($($arg:ty),*);)*) => {
        $(fn $method(self, $(_: $arg),*) -> Result<(), CodecError> {
            Err(not_a_struct())
        })*
    };
}

impl<'a, W: Write> serde::Serializer for &'a mut BusSplitter<W> {
    type Ok = ();
    type Error = CodecError;
    type SerializeSeq = Impossible<(), CodecError>;
    type SerializeTuple = Impossible<(), CodecError>;
    type SerializeTupleStruct = Impossible<(), CodecError>;
    type SerializeTupleVariant = Impossible<(), CodecError>;
    type SerializeMap = Impossible<(), CodecError>;
    type SerializeStruct = StructSink<'a, W>;
    type SerializeStructVariant = Impossible<(), CodecError>;

    fn serialize_struct(
        self,
        _name: &'static str,
        _len: usize,
    ) -> Result<StructSink<'a, W>, CodecError> {
        Ok(StructSink { splitter: self })
    }

    refuse_scalars! {
        serialize_bool(bool);
        serialize_i8(i8);
        serialize_i16(i16);
        serialize_i32(i32);
        serialize_i64(i64);
        serialize_u8(u8);
        serialize_u16(u16);
        serialize_u32(u32);
        serialize_u64(u64);
        serialize_f32(f32);
        serialize_f64(f64);
        serialize_char(char);
        serialize_str(&str);
        serialize_bytes(&[u8]);
        serialize_none();
        serialize_unit();
        serialize_unit_struct(&'static str);
        serialize_unit_variant(&'static str, u32, &'static str);
    }

    fn serialize_some<T: ?Sized + Serialize>(self, _: &T) -> Result<(), CodecError> {
        Err(not_a_struct())
    }

    fn serialize_newtype_struct<T: ?Sized + Serialize>(
        self,
        _: &'static str,
        _: &T,
    ) -> Result<(), CodecError> {
        Err(not_a_struct())
    }

    fn serialize_newtype_variant<T: ?Sized + Serialize>(
        self,
        _: &'static str,
        _: u32,
        _: &'static str,
        _: &T,
    ) -> Result<(), CodecError> {
        Err(not_a_struct())
    }

    fn serialize_seq(self, _: Option<usize>) -> Result<Self::SerializeSeq, CodecError> {
        Err(not_a_struct())
    }

    fn serialize_tuple(self, _: usize) -> Result<Self::SerializeTuple, CodecError> {
        Err(not_a_struct())
    }

    fn serialize_tuple_struct(
        self,
        _: &'static str,
        _: usize,
    ) -> Result<Self::SerializeTupleStruct, CodecError> {
        Err(not_a_struct())
    }

    fn serialize_tuple_variant(
        self,
        _: &'static str,
        _: u32,
        _: &'static str,
        _: usize,
    ) -> Result<Self::SerializeTupleVariant, CodecError> {
        Err(not_a_struct())
    }

    fn serialize_map(self, _: Option<usize>) -> Result<Self::SerializeMap, CodecError> {
        Err(not_a_struct())
    }

    fn serialize_struct_variant(
        self,
        _: &'static str,
        _: u32,
        _: &'static str,
        _: usize,
    ) -> Result<Self::SerializeStructVariant, CodecError> {
        Err(not_a_struct())
    }
}

/// Receives the struct's fields.
pub(crate) struct StructSink<'a, W: Write> {
    splitter: &'a mut BusSplitter<W>,
}

impl<W: Write> SerializeStruct for StructSink<'_, W> {
    type Ok = ();
    type Error = CodecError;

    fn serialize_field<T: ?Sized + Serialize>(
        &mut self,
        key: &'static str,
        value: &T,
    ) -> Result<(), CodecError> {
        let splitter = &mut *self.splitter;
        let spec = chunk::chunk_for_field(key);
        let field_error = |e: rmp_serde::encode::Error| CodecError(format!("field {key}: {e}"));
        match spec.payload {
            Payload::BusFields(fields) => {
                splitter.open(
                    spec,
                    u32::try_from(fields.len()).map_err(BusSplitter::<W>::io)?,
                )?;
                let Some(Sink::Streaming { body, written, .. }) = splitter.sink.as_mut() else {
                    return Err(CodecError("no streaming chunk".into()));
                };
                chunk::encode(key, body)
                    .and_then(|()| chunk::encode(value, body))
                    .map_err(field_error)?;
                *written += 1;
            }
            Payload::BusRest => {
                chunk::encode(key, &mut splitter.rest)
                    .and_then(|()| chunk::encode(value, &mut splitter.rest))
                    .map_err(field_error)?;
                splitter.rest_written += 1;
            }
            Payload::Value => return Err(CodecError(format!("field {key} maps to a value chunk"))),
        }
        Ok(())
    }

    fn end(self) -> Result<(), CodecError> {
        let splitter = self.splitter;
        splitter.close()?;
        // The catch-all was buffered, so its entry count is known now.
        let Some(Sink::Idle(writer)) = splitter.sink.take() else {
            return Err(CodecError("no chunk writer".into()));
        };
        let mut body = writer.stream(&chunk::BUS).map_err(BusSplitter::<W>::io)?;
        rmp::encode::write_map_len(&mut body, splitter.rest_written)
            .map_err(BusSplitter::<W>::io)?;
        body.write_all(&splitter.rest)
            .map_err(BusSplitter::<W>::io)?;
        splitter.rest = Vec::new();
        splitter.sink = Some(Sink::Idle(body.finish().map_err(BusSplitter::<W>::io)?));
        Ok(())
    }
}

/// One `Bus` chunk's field map, mid-read.
struct Part<R: Read> {
    spec: &'static ChunkSpec,
    deserializer: rmp_serde::Deserializer<rmp_serde::decode::ReadReader<PartSource<R>>>,
    remaining: u32,
}

impl<R: Read> Part<R> {
    fn open(spec: &'static ChunkSpec, mut source: PartSource<R>) -> Result<Self, CodecError> {
        let remaining = rmp::decode::read_map_len(&mut source).map_err(|e| {
            CodecError(format!(
                "{} chunk: expected a map of bus fields: {e}",
                tag_name(spec.tag)
            ))
        })?;
        let mut deserializer = rmp_serde::Deserializer::new(source);
        deserializer.set_max_depth(MAX_NESTING);
        Ok(Self {
            spec,
            deserializer,
            remaining,
        })
    }

    fn error(&self, what: &str, e: impl std::fmt::Display) -> CodecError {
        CodecError(format!("{} chunk, {what}: {e}", tag_name(self.spec.tag)))
    }

    /// Verify the payload ended with its last field and give the stream
    /// back if the part was reading it.
    fn close(self) -> Result<Option<R>, CodecError> {
        let name = tag_name(self.spec.tag);
        match self.deserializer.into_inner() {
            PartSource::Stream(body) => body
                .finish()
                .map(Some)
                .map_err(|e| CodecError(format!("{name} chunk: {e:#}"))),
            PartSource::Owned(cursor) => {
                let unread = cursor.get_ref().len() as u64 - cursor.position();
                if unread != 0 {
                    return Err(CodecError(format!(
                        "{name} chunk: {unread} unread bytes after its last field"
                    )));
                }
                Ok(None)
            }
        }
    }
}

/// Feeds the derived `Bus` visitor from a chunk stream, as a
/// `Deserializer` (for `Bus::deserialize`) that is also its own
/// `MapAccess`.
pub(crate) struct BusJoiner<'a, R: Read, S: ValueSink> {
    /// The stream, while no streaming part holds it.
    stream: Option<R>,
    current: Option<Part<R>>,
    sink: &'a mut S,
    migrations: &'a [Migration],
    seen: Vec<Tag>,
    ended: bool,
}

impl<'a, R: Read, S: ValueSink> BusJoiner<'a, R, S> {
    pub fn new(stream: R, migrations: &'a [Migration], sink: &'a mut S) -> Self {
        Self {
            stream: Some(stream),
            current: None,
            sink,
            migrations,
            seen: Vec::new(),
            ended: false,
        }
    }

    /// The tags met so far.
    pub fn seen(&self) -> &[Tag] {
        &self.seen
    }

    /// After `Bus::deserialize`: the stream must have reached its end
    /// marker.
    pub fn finish(self) -> anyhow::Result<()> {
        if !self.ended {
            anyhow::bail!("the bus was restored before the END marker was reached");
        }
        Ok(())
    }

    fn stream_error(what: &str, e: impl std::fmt::Display) -> CodecError {
        CodecError(format!("{what}: {e}"))
    }

    /// Pull the next chunk from the stream: open a `Bus` chunk as the
    /// current part, deliver a value chunk to the sink, skip an unknown
    /// one. `Ok(false)` at the end marker.
    fn pull(&mut self) -> Result<bool, CodecError> {
        let mut stream = self
            .stream
            .take()
            .ok_or_else(|| CodecError("the chunk stream is not available".into()))?;
        let header = match read_header(&mut stream) {
            Ok(header) => header,
            Err(e) => {
                return Err(Self::stream_error(
                    "reading save state chunks",
                    format!("{e:#}"),
                ))
            }
        };
        if header.tag == chunk::END {
            let result = finish_stream(&header, &mut stream);
            self.stream = Some(stream);
            result
                .map_err(|e| Self::stream_error("reading save state chunks", format!("{e:#}")))?;
            self.ended = true;
            return Ok(false);
        }
        if self.seen.contains(&header.tag) {
            self.stream = Some(stream);
            return Err(CodecError(format!(
                "duplicate {} chunk",
                tag_name(header.tag)
            )));
        }
        self.seen.push(header.tag);
        let name = tag_name(header.tag);
        let body = Body::open(&header, stream);
        let Some(spec) = chunk::spec_for(header.tag) else {
            let (len, stream) = body.skip().map_err(|e| {
                Self::stream_error(&format!("skipping {name} chunk"), format!("{e:#}"))
            })?;
            log::warn!(
                "save state: skipping unknown {name} chunk (version {}, {len} bytes)",
                header.version
            );
            self.stream = Some(stream);
            return Ok(true);
        };
        match spec.payload {
            Payload::Value => {
                let (payload, stream) = body.read_to_vec().map_err(|e| {
                    Self::stream_error(&format!("reading {name} chunk"), format!("{e:#}"))
                })?;
                self.stream = Some(stream);
                let payload = chunk::upgrade(spec, header.version, payload, self.migrations)
                    .map_err(|e| CodecError(format!("{e:#}")))?;
                self.sink.value(spec, payload).map_err(|e| {
                    Self::stream_error(
                        &format!("reading {name} chunk ({})", spec.name),
                        format!("{e:#}"),
                    )
                })?;
            }
            Payload::BusFields(_) | Payload::BusRest => {
                let source = if header.version == spec.version {
                    PartSource::Stream(body)
                } else {
                    let (payload, stream) = body.read_to_vec().map_err(|e| {
                        Self::stream_error(&format!("reading {name} chunk"), format!("{e:#}"))
                    })?;
                    self.stream = Some(stream);
                    let payload = chunk::upgrade(spec, header.version, payload, self.migrations)
                        .map_err(|e| CodecError(format!("{e:#}")))?;
                    PartSource::Owned(Cursor::new(payload))
                };
                self.current = Some(Part::open(spec, source)?);
            }
        }
        Ok(true)
    }
}

impl<'de, R: Read, S: ValueSink> serde::Deserializer<'de> for &mut BusJoiner<'_, R, S> {
    type Error = CodecError;

    fn deserialize_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        _fields: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, CodecError> {
        visitor.visit_map(&mut *self)
    }

    fn deserialize_any<V: Visitor<'de>>(self, _visitor: V) -> Result<V::Value, CodecError> {
        Err(CodecError(
            "bus chunks deserialize into a struct".to_string(),
        ))
    }

    serde::forward_to_deserialize_any! {
        bool i8 i16 i32 i64 u8 u16 u32 u64 f32 f64 char str string bytes
        byte_buf option unit unit_struct newtype_struct seq tuple tuple_struct
        map enum identifier ignored_any
    }
}

impl<'de, R: Read, S: ValueSink> MapAccess<'de> for BusJoiner<'_, R, S> {
    type Error = CodecError;

    fn next_key_seed<K: DeserializeSeed<'de>>(
        &mut self,
        seed: K,
    ) -> Result<Option<K::Value>, CodecError> {
        loop {
            if let Some(part) = self.current.as_mut() {
                if part.remaining > 0 {
                    part.remaining -= 1;
                    let key: String = serde::Deserialize::deserialize(&mut part.deserializer)
                        .map_err(|e| part.error("field name", e))?;
                    return seed.deserialize(StrDeserializer::new(&key)).map(Some);
                }
                let part = self.current.take().expect("a current part");
                if let Some(stream) = part.close()? {
                    self.stream = Some(stream);
                }
            }
            if self.ended || !self.pull()? {
                return Ok(None);
            }
        }
    }

    fn next_value_seed<V: DeserializeSeed<'de>>(
        &mut self,
        seed: V,
    ) -> Result<V::Value, CodecError> {
        let part = self
            .current
            .as_mut()
            .ok_or_else(|| CodecError("a value was requested before its key".to_string()))?;
        seed.deserialize(&mut part.deserializer)
            .map_err(|e| part.error("field value", e))
    }
}
