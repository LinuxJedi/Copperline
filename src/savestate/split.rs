// SPDX-License-Identifier: GPL-3.0-or-later

//! Splitting the `Bus` into subsystem chunks, and joining them back.
//!
//! `Bus` keeps its derived serde impls. [`BusSplitter`] is a `Serializer`
//! that accepts only a struct and files each field into the chunk that
//! claims it (`ChunkSpec::payload`), so a field added to `Bus` lands in the
//! catch-all `BUS ` chunk with no table edit. [`BusJoiner`] is the
//! `Deserializer` counterpart: it presents the chunks' field maps to the
//! derived visitor as one map, so `Bus::deserialize` sees exactly what a
//! single-map encoding would have given it (any order, unknown fields
//! ignored, missing ones defaulted where the struct allows).

use serde::de::value::BorrowedStrDeserializer;
use serde::de::{DeserializeSeed, MapAccess, Visitor};
use serde::ser::{Impossible, Serialize, SerializeStruct};

use super::chunk::{self, tag_name, BusPart, ChunkSpec, CodecError, Payload};

struct Bucket {
    spec: &'static ChunkSpec,
    entries: u32,
    bytes: Vec<u8>,
}

/// Serializes one struct's fields into per-chunk field maps.
pub(crate) struct BusSplitter {
    buckets: Vec<Bucket>,
    rest: usize,
}

impl BusSplitter {
    fn new() -> Self {
        let buckets: Vec<Bucket> = chunk::bus_chunks()
            .map(|spec| Bucket {
                spec,
                entries: 0,
                bytes: Vec::new(),
            })
            .collect();
        let rest = buckets
            .iter()
            .position(|bucket| matches!(bucket.spec.payload, Payload::BusRest))
            .expect("a catch-all bus chunk");
        Self { buckets, rest }
    }

    fn bucket_for(&mut self, field: &str) -> &mut Bucket {
        let index = self
            .buckets
            .iter()
            .position(|bucket| {
                matches!(bucket.spec.payload, Payload::BusFields(fields) if fields.contains(&field))
            })
            .unwrap_or(self.rest);
        &mut self.buckets[index]
    }

    /// Serialize `value` (a struct) into one field-map payload per `Bus`
    /// chunk, in file order. Every chunk is produced, empty or not.
    pub fn split<T: Serialize + ?Sized>(
        value: &T,
    ) -> Result<Vec<(&'static ChunkSpec, Vec<u8>)>, CodecError> {
        let mut splitter = Self::new();
        value.serialize(&mut splitter)?;
        splitter
            .buckets
            .into_iter()
            .map(|bucket| {
                let mut payload = Vec::with_capacity(bucket.bytes.len() + 5);
                rmp::encode::write_map_len(&mut payload, bucket.entries)
                    .map_err(|e| CodecError(e.to_string()))?;
                payload.extend_from_slice(&bucket.bytes);
                Ok((bucket.spec, payload))
            })
            .collect()
    }
}

/// Receives the struct's fields.
pub(crate) struct StructSink<'a> {
    splitter: &'a mut BusSplitter,
}

impl SerializeStruct for StructSink<'_> {
    type Ok = ();
    type Error = CodecError;

    fn serialize_field<T: ?Sized + Serialize>(
        &mut self,
        key: &'static str,
        value: &T,
    ) -> Result<(), CodecError> {
        let bucket = self.splitter.bucket_for(key);
        let encoded = chunk::encode(key, &mut bucket.bytes)
            .and_then(|()| chunk::encode(value, &mut bucket.bytes));
        encoded.map_err(|e| CodecError(format!("field {key}: {e}")))?;
        bucket.entries += 1;
        Ok(())
    }

    fn end(self) -> Result<(), CodecError> {
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

impl<'a> serde::Serializer for &'a mut BusSplitter {
    type Ok = ();
    type Error = CodecError;
    type SerializeSeq = Impossible<(), CodecError>;
    type SerializeTuple = Impossible<(), CodecError>;
    type SerializeTupleStruct = Impossible<(), CodecError>;
    type SerializeTupleVariant = Impossible<(), CodecError>;
    type SerializeMap = Impossible<(), CodecError>;
    type SerializeStruct = StructSink<'a>;
    type SerializeStructVariant = Impossible<(), CodecError>;

    fn serialize_struct(
        self,
        _name: &'static str,
        _len: usize,
    ) -> Result<StructSink<'a>, CodecError> {
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

/// The chunks that make up one `Bus`, as a `Deserializer` for the derived
/// `Bus` visitor.
pub(crate) struct BusJoiner<'de> {
    parts: &'de [BusPart<'de>],
}

impl<'de> BusJoiner<'de> {
    pub fn new(parts: &'de [BusPart<'de>]) -> Self {
        Self { parts }
    }
}

impl<'de> serde::Deserializer<'de> for BusJoiner<'de> {
    type Error = CodecError;

    fn deserialize_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        _fields: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, CodecError> {
        visitor.visit_map(JoinMap {
            parts: self.parts.iter(),
            current: None,
        })
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

/// One chunk's field map, mid-read.
struct Part<'de> {
    spec: &'static ChunkSpec,
    deserializer: rmp_serde::Deserializer<rmp_serde::decode::ReadRefReader<'de, [u8]>>,
    remaining: u32,
}

impl<'de> Part<'de> {
    fn open(spec: &'static ChunkSpec, bytes: &'de [u8]) -> Result<Self, CodecError> {
        chunk::check_shape(bytes)
            .map_err(|e| CodecError(format!("{} chunk: {e:#}", tag_name(spec.tag))))?;
        let mut cursor = bytes;
        let remaining = rmp::decode::read_map_len(&mut cursor).map_err(|e| {
            CodecError(format!(
                "{} chunk: expected a map of bus fields: {e}",
                tag_name(spec.tag)
            ))
        })?;
        Ok(Self {
            spec,
            deserializer: rmp_serde::Deserializer::from_read_ref(cursor),
            remaining,
        })
    }

    fn error(&self, what: &str, e: impl std::fmt::Display) -> CodecError {
        CodecError(format!("{} chunk, {what}: {e}", tag_name(self.spec.tag)))
    }
}

/// The concatenation of every chunk's field map.
struct JoinMap<'de> {
    parts: std::slice::Iter<'de, BusPart<'de>>,
    current: Option<Part<'de>>,
}

impl<'de> MapAccess<'de> for JoinMap<'de> {
    type Error = CodecError;

    fn next_key_seed<K: DeserializeSeed<'de>>(
        &mut self,
        seed: K,
    ) -> Result<Option<K::Value>, CodecError> {
        loop {
            if let Some(part) = self.current.as_mut() {
                if part.remaining > 0 {
                    part.remaining -= 1;
                    let key: &'de str = serde::Deserialize::deserialize(&mut part.deserializer)
                        .map_err(|e| part.error("field name", e))?;
                    return seed
                        .deserialize(BorrowedStrDeserializer::new(key))
                        .map(Some);
                }
                // `check_shape` proved the payload is exactly one map, so
                // its last entry is also the end of the chunk.
                self.current = None;
            }
            match self.parts.next() {
                None => return Ok(None),
                Some((spec, payload)) => self.current = Some(Part::open(spec, payload.as_ref())?),
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
