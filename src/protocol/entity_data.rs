//! Entity metadata: actor data values keyed by data key (gophertunnel `EntityMetadata`).

use std::collections::BTreeMap;
use std::mem::size_of;

use super::common::BlockPos;
use super::consts::EntityDataType;
use super::io::{Error, MAX_LIST_LEN, Reader, Result, Writer, length};
use crate::nbt::{Compound, Encoding};

/// Entity metadata: values by data key, written in ascending key order like gophertunnel.
pub type EntityMetadata = BTreeMap<u32, MetadataValue>;

/// One entity metadata value; the variant is the wire data type (gophertunnel `EntityDataType*`).
#[derive(Debug, Clone, PartialEq)]
pub enum MetadataValue {
    /// Type 0: a byte.
    Byte(u8),
    /// Type 1: a little-endian int16.
    Int16(i16),
    /// Type 2: a varint32.
    Int32(i32),
    /// Type 3: a little-endian float32.
    Float32(f32),
    /// Type 4: a string.
    String(String),
    /// Type 5: a network little-endian NBT compound.
    CompoundTag(Compound),
    /// Type 6: a block position.
    BlockPos(BlockPos),
    /// Type 7: a varint64.
    Int64(i64),
    /// Type 8: three float32s.
    Vec3([f32; 3]),
}

impl MetadataValue {
    fn data_type(&self) -> EntityDataType {
        match self {
            Self::Byte(_) => EntityDataType::Byte,
            Self::Int16(_) => EntityDataType::Int16,
            Self::Int32(_) => EntityDataType::Int32,
            Self::Float32(_) => EntityDataType::Float32,
            Self::String(_) => EntityDataType::String,
            Self::CompoundTag(_) => EntityDataType::CompoundTag,
            Self::BlockPos(_) => EntityDataType::BlockPos,
            Self::Int64(_) => EntityDataType::Int64,
            Self::Vec3(_) => EntityDataType::Vec3,
        }
    }
}

impl Reader<'_> {
    /// `EntityMetadata`: at most [`MAX_LIST_LEN`] entries; a repeated key keeps its last value.
    pub(crate) fn entity_metadata(&mut self) -> Result<EntityMetadata> {
        let count = self.varuint32()?;
        let count = self.limit(
            count.into(),
            MAX_LIST_LEN,
            size_of::<(u32, MetadataValue)>(),
        )?;
        let mut map = EntityMetadata::new();
        for _ in 0..count {
            let key = self.varuint32()?;
            let data_type = self.varuint32()?;
            let legacy_type = self.uint8()?;
            if data_type != u32::from(legacy_type) {
                return Err(Error::InvalidValue {
                    field: "MetadataValue type",
                    reason: "legacy type byte differs from the type",
                });
            }
            let value = match EntityDataType::try_from(data_type)? {
                EntityDataType::Byte => MetadataValue::Byte(self.uint8()?),
                EntityDataType::Int16 => MetadataValue::Int16(self.int16()?),
                EntityDataType::Int32 => MetadataValue::Int32(self.varint32()?),
                EntityDataType::Float32 => MetadataValue::Float32(self.float32()?),
                EntityDataType::String => MetadataValue::String(self.string()?),
                EntityDataType::CompoundTag => {
                    MetadataValue::CompoundTag(self.nbt(Encoding::NetworkLittleEndian)?)
                }
                EntityDataType::BlockPos => MetadataValue::BlockPos(self.block_pos()?),
                EntityDataType::Int64 => MetadataValue::Int64(self.varint64()?),
                EntityDataType::Vec3 => MetadataValue::Vec3(self.vec3()?),
            };
            map.insert(key, value);
        }
        Ok(map)
    }
}

impl Writer {
    /// `EntityMetadata`
    pub(crate) fn entity_metadata(&mut self, x: &EntityMetadata) -> Result<()> {
        self.varuint32(length(x.len())?);
        for (&key, value) in x {
            self.varuint32(key);
            let data_type = u32::from(value.data_type());
            self.varuint32(data_type);
            // The legacy type byte repeats the type; every type is below 256.
            self.uint8(u8::try_from(data_type).map_err(|_| Error::InvalidValue {
                field: "MetadataValue type",
                reason: "over 255",
            })?);
            match value {
                MetadataValue::Byte(v) => self.uint8(*v),
                MetadataValue::Int16(v) => self.int16(*v),
                MetadataValue::Int32(v) => self.varint32(*v),
                MetadataValue::Float32(v) => self.float32(*v),
                MetadataValue::String(v) => self.string(v)?,
                MetadataValue::CompoundTag(v) => self.nbt(v, Encoding::NetworkLittleEndian)?,
                MetadataValue::BlockPos(v) => self.block_pos(*v),
                MetadataValue::Int64(v) => self.varint64(*v),
                MetadataValue::Vec3(v) => self.vec3(*v),
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nbt::Value;
    use crate::protocol::fuzz_round_trip;

    fn encode(x: &EntityMetadata) -> Vec<u8> {
        let mut w = Writer::new(Vec::new(), 0);
        w.entity_metadata(x).unwrap();
        w.into_inner()
    }

    fn every_type() -> EntityMetadata {
        EntityMetadata::from([
            (0, MetadataValue::Byte(5)),
            (1, MetadataValue::Int16(-2)),
            (2, MetadataValue::Int32(-1)),
            (3, MetadataValue::Float32(0.5)),
            (4, MetadataValue::String("x".to_owned())),
            (
                5,
                MetadataValue::CompoundTag(Compound::from([("a".to_owned(), Value::Int(1))])),
            ),
            (6, MetadataValue::BlockPos([1, 2, 3])),
            (7, MetadataValue::Int64(i64::MIN)),
            (300, MetadataValue::Vec3([1.0, 2.0, 3.0])),
        ])
    }

    #[test]
    fn entries_are_written_in_key_order_like_gophertunnel() {
        let x = EntityMetadata::from([
            (4, MetadataValue::String("x".to_owned())),
            (0, MetadataValue::Byte(5)),
            (2, MetadataValue::Int32(-1)),
        ]);
        let bytes = encode(&x);
        assert_eq!(bytes, [3, 0, 0, 0, 5, 2, 2, 2, 1, 4, 4, 4, 1, b'x']);
        assert_eq!(Reader::new(&bytes, 1 << 20, 0).entity_metadata(), Ok(x));
    }

    #[test]
    fn every_type_round_trips() {
        let bytes = encode(&every_type());
        let mut r = Reader::new(&bytes, 1 << 20, 0);
        assert_eq!(r.entity_metadata(), Ok(every_type()));
        assert!(r.remaining().is_empty());
    }

    #[test]
    fn a_repeated_key_keeps_the_last_value() {
        let bytes = [2, 9, 0, 0, 1, 9, 0, 0, 2];
        let got = Reader::new(&bytes, 1 << 20, 0).entity_metadata();
        assert_eq!(got, Ok(EntityMetadata::from([(9, MetadataValue::Byte(2))])));
    }

    #[test]
    fn bad_types_are_rejected() {
        let mismatch = [1, 0, 2, 0, 5];
        assert!(
            Reader::new(&mismatch, 1 << 20, 0)
                .entity_metadata()
                .is_err()
        );
        let unknown = [1, 0, 9, 9];
        assert_eq!(
            Reader::new(&unknown, 1 << 20, 0).entity_metadata(),
            Err(Error::UnknownEnum {
                name: "entity data type",
                value: 9
            })
        );
    }

    #[test]
    fn counts_over_the_limit_are_size_errors() {
        let mut w = Writer::new(Vec::new(), 0);
        w.varuint32(1025);
        let mut bytes = w.into_inner();
        bytes.extend([0; 4100]);
        assert_eq!(
            Reader::new(&bytes, 1 << 20, 0).entity_metadata(),
            Err(Error::Size)
        );
        assert_eq!(
            Reader::new(&[5, 0, 0, 0, 1], 1 << 20, 0).entity_metadata(),
            Err(Error::Size)
        );
        let bytes = encode(&every_type());
        assert_eq!(
            Reader::new(&bytes, 64, 0).entity_metadata(),
            Err(Error::Size)
        );
    }

    #[test]
    #[expect(
        clippy::redundant_closure_for_method_calls,
        reason = "consistency: a method path fixes the reader lifetime; the closure keeps it generic"
    )]
    fn mutated_input_never_panics() {
        let decoded = fuzz_round_trip(
            &encode(&every_type()),
            0,
            |r| r.entity_metadata(),
            |w, x| w.entity_metadata(x),
        );
        assert!(decoded > 0);
    }
}
