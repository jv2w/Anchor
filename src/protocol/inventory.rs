//! Item stacks and their wire forms: `Item`, `ItemInstance`, `StackRequestItem` and the shared
//! item user data blob, plus the education edition `MaterialReducer`.

use super::io::{Codec, Error, Reader, Result, Writer, length};
use crate::nbt::{Compound, Encoding};

/// The identifier of the item whose user data carries a blocking tick in `StackRequestItem`.
const SHIELD_IDENTIFIER: &str = "minecraft:shield";

/// An item's network ID and metadata value (gophertunnel `ItemType`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct ItemType {
    /// The numerical network ID of the item; 0 is air.
    pub network_id: i32,
    /// The damage or variant value of the item.
    pub metadata_value: u32,
}

/// An item stack (gophertunnel `ItemStack`).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ItemStack {
    /// The item's network ID and metadata value.
    pub item_type: ItemType,
    /// The runtime ID of the block the item places, or 0.
    pub block_runtime_id: i32,
    /// The number of items in the stack.
    pub count: u16,
    /// The item's NBT; empty when it has none.
    pub nbt_data: Compound,
    /// Block names the item may be placed on in adventure mode.
    pub can_be_placed_on: Vec<String>,
    /// Block names the item may break in adventure mode.
    pub can_break: Vec<String>,
    /// The tick the shield started blocking; only on the wire for shields.
    pub blocking_tick: i64,
}

/// An item stack with its stack network ID (gophertunnel `ItemInstance`).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ItemInstance {
    /// The server-assigned stack ID; 0 when there is none.
    pub stack_network_id: i32,
    /// The item stack.
    pub stack: ItemStack,
}

/// An item in the descriptor-based form of deprecated craft-result actions (gophertunnel
/// `StackRequestItem`).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct StackRequestItem {
    /// The item's identifier, such as `minecraft:stick`; empty for no item.
    pub identifier: String,
    /// The damage or variant value of the item.
    pub metadata_value: u32,
    /// The runtime ID of the block the item places, or 0.
    pub block_runtime_id: i32,
    /// The number of items in the stack.
    pub count: u16,
    /// The item's NBT; empty when it has none.
    pub nbt_data: Compound,
    /// Block names the item may be placed on in adventure mode.
    pub can_be_placed_on: Vec<String>,
    /// Block names the item may break in adventure mode.
    pub can_break: Vec<String>,
    /// The tick the shield started blocking; only on the wire for shields.
    pub blocking_tick: i64,
}

/// One output of a material reducer craft (gophertunnel `MaterialReducerOutput`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct MaterialReducerOutput {
    /// The network ID of the output.
    pub network_id: i32,
    /// The quantity of the output.
    pub count: i32,
}

/// A craft in an education edition material reducer (gophertunnel `MaterialReducer`).
///
/// The input is one varint32 `network_id << 16 | metadata_value`. gophertunnel reads it as
/// `mix << 16` and `mix & 0x7fff`, which does not survive a round trip; Anchor reads
/// `mix >> 16` and `mix & 0xffff`, so every input re-encodes to the same bytes.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct MaterialReducer {
    /// The starting item; its network ID fits an int16 and its metadata a uint16.
    pub input_item: ItemType,
    /// The items the craft gives.
    pub outputs: Vec<MaterialReducerOutput>,
}

/// The fields of the item user data blob.
#[derive(Default)]
struct UserData {
    nbt_data: Compound,
    can_be_placed_on: Vec<String>,
    can_break: Vec<String>,
    blocking_tick: i64,
}

impl Reader<'_> {
    /// Reads the item user data blob. Like gophertunnel, an int16 marker other than 0 and -1
    /// is followed directly by NBT, and bytes after the known fields are ignored.
    fn item_user_data(&mut self, shield: bool) -> Result<UserData> {
        let len = self.varuint32()?;
        let data = self.raw(len.into())?;
        if data.is_empty() {
            return Ok(UserData::default());
        }
        self.nested(data, |r| {
            let mut x = UserData::default();
            match r.int16()? {
                0 => {}
                -1 => {
                    let version = r.uint8()?;
                    if version != 1 {
                        return Err(Error::UnknownEnum {
                            name: "item user data version",
                            value: version.into(),
                        });
                    }
                    x.nbt_data = r.nbt(Encoding::LittleEndian)?;
                }
                _ => x.nbt_data = r.nbt(Encoding::LittleEndian)?,
            }
            let count = r.uint32()?;
            x.can_be_placed_on = r.list(count.into(), Reader::string_utf)?;
            let count = r.uint32()?;
            x.can_break = r.list(count.into(), Reader::string_utf)?;
            if shield {
                x.blocking_tick = r.int64()?;
            }
            Ok(x)
        })
    }

    /// `Item`
    pub(crate) fn item(&mut self) -> Result<ItemStack> {
        let network_id = self.varint32()?;
        let count = self.uint16()?;
        let metadata_value = self.varuint32()?;
        let block_runtime_id = self.varint32()?;
        let data = self.item_user_data(network_id == self.shield_id())?;
        Ok(ItemStack {
            item_type: ItemType {
                network_id,
                metadata_value,
            },
            block_runtime_id,
            count,
            nbt_data: data.nbt_data,
            can_be_placed_on: data.can_be_placed_on,
            can_break: data.can_break,
            blocking_tick: data.blocking_tick,
        })
    }

    /// `ItemInstance`
    pub(crate) fn item_instance(&mut self) -> Result<ItemInstance> {
        let network_id = self.int16()?.into();
        let count = self.uint16()?;
        let metadata_value = self.varuint32()?;
        let stack_network_id = if self.bool()? { self.varint32()? } else { 0 };
        let block_runtime_id = self.varuint32()?.cast_signed();
        let data = self.item_user_data(network_id == self.shield_id())?;
        Ok(ItemInstance {
            stack_network_id,
            stack: ItemStack {
                item_type: ItemType {
                    network_id,
                    metadata_value,
                },
                block_runtime_id,
                count,
                nbt_data: data.nbt_data,
                can_be_placed_on: data.can_be_placed_on,
                can_break: data.can_break,
                blocking_tick: data.blocking_tick,
            },
        })
    }

    /// `StackRequestItem`
    pub(crate) fn stack_request_item(&mut self) -> Result<StackRequestItem> {
        let variant = self.varuint32()?;
        // gophertunnel reads the legacy variant byte without checking it.
        self.uint8()?;
        let (identifier, metadata_value) = match variant {
            0 => (String::new(), 0),
            1 => (self.string()?, self.varint32()?.cast_unsigned()),
            v => {
                return Err(Error::UnknownEnum {
                    name: "stack request item descriptor",
                    value: v.into(),
                });
            }
        };
        let count = self.int16()?.cast_unsigned();
        let block_runtime_id = self.varuint32()?.cast_signed();
        let data = self.item_user_data(identifier == SHIELD_IDENTIFIER)?;
        Ok(StackRequestItem {
            identifier,
            metadata_value,
            block_runtime_id,
            count,
            nbt_data: data.nbt_data,
            can_be_placed_on: data.can_be_placed_on,
            can_break: data.can_break,
            blocking_tick: data.blocking_tick,
        })
    }

    /// `MaterialReducer`
    pub(crate) fn material_reducer(&mut self) -> Result<MaterialReducer> {
        let mix = self.varint32()?;
        let count = self.varuint32()?;
        Ok(MaterialReducer {
            input_item: ItemType {
                network_id: mix >> 16,
                metadata_value: (mix & 0xffff).cast_unsigned(),
            },
            outputs: self.list(count.into(), MaterialReducerOutput::decode)?,
        })
    }
}

impl Writer {
    /// Writes the item user data blob; `blocking_tick` is `Some` for shields.
    fn item_user_data(
        &mut self,
        nbt_data: &Compound,
        can_be_placed_on: &[String],
        can_break: &[String],
        blocking_tick: Option<i64>,
    ) -> Result<()> {
        self.length_prefixed(|w| {
            if nbt_data.is_empty() {
                w.int16(0);
            } else {
                w.int16(-1);
                w.uint8(1);
                w.nbt(nbt_data, Encoding::LittleEndian)?;
            }
            for list in [can_be_placed_on, can_break] {
                w.uint32(length(list.len())?);
                for name in list {
                    w.string_utf(name)?;
                }
            }
            if let Some(tick) = blocking_tick {
                w.int64(tick);
            }
            Ok(())
        })
    }

    /// Writes the user data of a stack, or an empty blob for air.
    fn stack_user_data(&mut self, x: &ItemStack) -> Result<()> {
        if x.item_type.network_id == 0 {
            self.varuint32(0);
            return Ok(());
        }
        let shield = x.item_type.network_id == self.shield_id();
        self.item_user_data(
            &x.nbt_data,
            &x.can_be_placed_on,
            &x.can_break,
            shield.then_some(x.blocking_tick),
        )
    }

    /// `Item`
    pub(crate) fn item(&mut self, x: &ItemStack) -> Result<()> {
        self.varint32(x.item_type.network_id);
        self.uint16(x.count);
        self.varuint32(x.item_type.metadata_value);
        self.varint32(x.block_runtime_id);
        self.stack_user_data(x)
    }

    /// `ItemInstance`: fails if the network ID does not fit an int16.
    pub(crate) fn item_instance(&mut self, x: &ItemInstance) -> Result<()> {
        let stack = &x.stack;
        let network_id =
            i16::try_from(stack.item_type.network_id).map_err(|_| Error::InvalidValue {
                field: "item instance network id",
                reason: "outside int16",
            })?;
        self.int16(network_id);
        self.uint16(stack.count);
        self.varuint32(stack.item_type.metadata_value);
        self.bool(x.stack_network_id != 0);
        if x.stack_network_id != 0 {
            self.varint32(x.stack_network_id);
        }
        self.varuint32(stack.block_runtime_id.cast_unsigned());
        self.stack_user_data(stack)
    }

    /// `StackRequestItem`
    pub(crate) fn stack_request_item(&mut self, x: &StackRequestItem) -> Result<()> {
        let has_item = !x.identifier.is_empty();
        self.varuint32(has_item.into());
        self.uint8(has_item.into());
        if has_item {
            self.string(&x.identifier)?;
            self.varint32(x.metadata_value.cast_signed());
        }
        self.int16(x.count.cast_signed());
        self.varuint32(x.block_runtime_id.cast_unsigned());
        if !has_item {
            self.varuint32(0);
            return Ok(());
        }
        let shield = x.identifier == SHIELD_IDENTIFIER;
        self.item_user_data(
            &x.nbt_data,
            &x.can_be_placed_on,
            &x.can_break,
            shield.then_some(x.blocking_tick),
        )
    }

    /// `MaterialReducer`: fails if the input's network ID does not fit an int16 or its
    /// metadata a uint16.
    pub(crate) fn material_reducer(&mut self, x: &MaterialReducer) -> Result<()> {
        let id = i16::try_from(x.input_item.network_id);
        let meta = u16::try_from(x.input_item.metadata_value);
        let (Ok(id), Ok(meta)) = (id, meta) else {
            return Err(Error::InvalidValue {
                field: "material reducer input",
                reason: "network id outside int16 or metadata outside uint16",
            });
        };
        self.varint32((i32::from(id) << 16) | i32::from(meta));
        self.varuint32(length(x.outputs.len())?);
        for output in &x.outputs {
            output.encode(self)?;
        }
        Ok(())
    }
}

impl Codec for ItemInstance {
    fn decode(r: &mut Reader<'_>) -> Result<Self> {
        r.item_instance()
    }

    fn encode(&self, w: &mut Writer) -> Result<()> {
        w.item_instance(self)
    }
}

impl Codec for StackRequestItem {
    fn decode(r: &mut Reader<'_>) -> Result<Self> {
        r.stack_request_item()
    }

    fn encode(&self, w: &mut Writer) -> Result<()> {
        w.stack_request_item(self)
    }
}

impl Codec for MaterialReducer {
    fn decode(r: &mut Reader<'_>) -> Result<Self> {
        r.material_reducer()
    }

    fn encode(&self, w: &mut Writer) -> Result<()> {
        w.material_reducer(self)
    }
}

impl Codec for MaterialReducerOutput {
    fn decode(r: &mut Reader<'_>) -> Result<Self> {
        Ok(Self {
            network_id: r.varint32()?,
            count: r.varint32()?,
        })
    }

    fn encode(&self, w: &mut Writer) -> Result<()> {
        w.varint32(self.network_id);
        w.varint32(self.count);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nbt::Value;
    use crate::protocol::fuzz_round_trip;

    const SHIELD: i32 = 357;
    const BUDGET: usize = 1 << 20;

    fn shield_stack() -> ItemStack {
        ItemStack {
            item_type: ItemType {
                network_id: SHIELD,
                metadata_value: 0,
            },
            count: 1,
            can_break: vec!["a".to_owned()],
            blocking_tick: 0x0102,
            ..ItemStack::default()
        }
    }

    fn rich_stack() -> ItemStack {
        ItemStack {
            item_type: ItemType {
                network_id: 5,
                metadata_value: 3,
            },
            block_runtime_id: -7,
            count: 64,
            nbt_data: Compound::from([("x".to_owned(), Value::Int(1))]),
            can_be_placed_on: vec!["minecraft:stone".to_owned()],
            can_break: vec!["minecraft:dirt".to_owned(), "b".to_owned()],
            blocking_tick: 0,
        }
    }

    fn write_item(x: &ItemStack, shield_id: i32) -> Vec<u8> {
        let mut w = Writer::new(Vec::new(), shield_id);
        w.item(x).unwrap();
        w.into_inner()
    }

    #[test]
    fn a_shield_item_matches_gophertunnel_bytes() {
        let bytes = write_item(&shield_stack(), SHIELD);
        let expected = [
            0xca, 0x05, // network id 357
            0x01, 0x00, // count
            0x00, // metadata
            0x00, // block runtime id
            0x15, // user data length 21
            0x00, 0x00, // no nbt
            0x00, 0x00, 0x00, 0x00, // can be placed on: none
            0x01, 0x00, 0x00, 0x00, 0x01, 0x00, b'a', // can break: "a"
            0x02, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // blocking tick
        ];
        assert_eq!(bytes, expected);
        assert_eq!(
            Reader::new(&bytes, BUDGET, SHIELD).item(),
            Ok(shield_stack())
        );
        // Without the shield ID the blocking tick is not read and the extra bytes are ignored.
        let other = Reader::new(&bytes, BUDGET, 0).item().unwrap();
        assert_eq!(other.blocking_tick, 0);
    }

    #[test]
    fn item_nbt_uses_the_little_endian_marker() {
        let bytes = write_item(&rich_stack(), SHIELD);
        let user_data = &bytes[6..];
        assert_eq!(
            user_data[..17],
            [
                0xff, 0xff, 0x01, 0x0a, 0x00, 0x00, 0x03, 0x01, 0x00, b'x', 0x01, 0x00, 0x00, 0x00,
                0x00, 0x01, 0x00
            ]
        );
        assert_eq!(Reader::new(&bytes, BUDGET, SHIELD).item(), Ok(rich_stack()));
    }

    #[test]
    fn air_writes_an_empty_user_data_blob() {
        let air = ItemStack {
            count: 3,
            nbt_data: rich_stack().nbt_data,
            ..ItemStack::default()
        };
        assert_eq!(write_item(&air, SHIELD), [0, 3, 0, 0, 0, 0]);
    }

    #[test]
    fn item_instances_round_trip() {
        for stack_network_id in [0, -3] {
            let x = ItemInstance {
                stack_network_id,
                stack: rich_stack(),
            };
            let mut w = Writer::new(Vec::new(), SHIELD);
            w.item_instance(&x).unwrap();
            let bytes = w.into_inner();
            assert_eq!(bytes[..2], 5i16.to_le_bytes());
            assert_eq!(Reader::new(&bytes, BUDGET, SHIELD).item_instance(), Ok(x));
        }
        let wide = ItemInstance {
            stack_network_id: 0,
            stack: ItemStack {
                item_type: ItemType {
                    network_id: 40_000,
                    metadata_value: 0,
                },
                ..ItemStack::default()
            },
        };
        assert!(Writer::new(Vec::new(), 0).item_instance(&wide).is_err());
    }

    #[test]
    fn stack_request_items_round_trip() {
        let x = StackRequestItem {
            identifier: SHIELD_IDENTIFIER.to_owned(),
            metadata_value: 2,
            block_runtime_id: 9,
            count: 1,
            blocking_tick: 77,
            ..StackRequestItem::default()
        };
        let mut w = Writer::new(Vec::new(), 0);
        w.stack_request_item(&x).unwrap();
        let bytes = w.into_inner();
        assert_eq!(bytes[..3], [1, 1, 16]);
        assert_eq!(Reader::new(&bytes, BUDGET, 0).stack_request_item(), Ok(x));
        let empty = StackRequestItem::default();
        let mut w = Writer::new(Vec::new(), 0);
        w.stack_request_item(&empty).unwrap();
        assert_eq!(w.into_inner(), [0, 0, 0, 0, 0, 0]);
        assert!(
            Reader::new(&[2, 2, 0, 0, 0, 0], BUDGET, 0)
                .stack_request_item()
                .is_err()
        );
    }

    #[test]
    fn material_reducers_round_trip_every_input() {
        let x = MaterialReducer {
            input_item: ItemType {
                network_id: -2,
                metadata_value: 0xffff,
            },
            outputs: vec![MaterialReducerOutput {
                network_id: 1,
                count: 2,
            }],
        };
        let mut w = Writer::new(Vec::new(), 0);
        x.encode(&mut w).unwrap();
        let bytes = w.into_inner();
        let mut r = Reader::new(&bytes, BUDGET, 0);
        assert_eq!(r.varint32(), Ok((-2 << 16) | 0xffff));
        assert_eq!(
            MaterialReducer::decode(&mut Reader::new(&bytes, BUDGET, 0)),
            Ok(x)
        );
        let wide = MaterialReducer {
            input_item: ItemType {
                network_id: 1 << 15,
                metadata_value: 0,
            },
            outputs: Vec::new(),
        };
        assert!(wide.encode(&mut Writer::new(Vec::new(), 0)).is_err());
    }

    #[test]
    fn user_data_limits_and_versions_are_enforced() {
        // Version 2 after the -1 marker.
        let bytes = [2, 1, 0, 0, 0, 3, 0xff, 0xff, 2];
        assert!(Reader::new(&bytes, BUDGET, 0).item().is_err());
        // 1025 can-break names.
        let mut blob = vec![0, 0, 0, 0, 0, 0, 0x01, 0x04, 0, 0];
        blob.extend([0; 2050]);
        let mut w = Writer::new(vec![2, 1, 0, 0, 0], 0);
        w.byte_slice(&blob).unwrap();
        let bytes = w.into_inner();
        assert_eq!(Reader::new(&bytes, BUDGET, 0).item(), Err(Error::Size));
        // A blob longer than the input.
        assert_eq!(
            Reader::new(&[2, 1, 0, 0, 0, 9, 0], BUDGET, 0).item(),
            Err(Error::Size)
        );
        // A name longer than an int16 cannot be written.
        let long = ItemStack {
            can_break: vec!["a".repeat(0x8000)],
            ..rich_stack()
        };
        assert_eq!(Writer::new(Vec::new(), 0).item(&long), Err(Error::Size));
    }

    #[test]
    #[expect(
        clippy::redundant_closure_for_method_calls,
        reason = "consistency: a method path fixes the reader lifetime; the closure keeps it generic"
    )]
    fn mutated_input_never_panics() {
        let seed = write_item(&rich_stack(), SHIELD);
        assert!(fuzz_round_trip(&seed, SHIELD, |r| r.item(), |w, x| w.item(x)) > 0);
        let seed = write_item(&shield_stack(), SHIELD);
        assert!(fuzz_round_trip(&seed, SHIELD, |r| r.item(), |w, x| w.item(x)) > 0);
        let mut w = Writer::new(Vec::new(), SHIELD);
        w.item_instance(&ItemInstance {
            stack_network_id: 4,
            stack: rich_stack(),
        })
        .unwrap();
        let seed = w.into_inner();
        let decoded = fuzz_round_trip(
            &seed,
            SHIELD,
            ItemInstance::decode,
            |w: &mut Writer, x: &ItemInstance| x.encode(w),
        );
        assert!(decoded > 0);
        let mut w = Writer::new(Vec::new(), 0);
        w.stack_request_item(&StackRequestItem {
            identifier: "minecraft:stone".to_owned(),
            count: 2,
            nbt_data: rich_stack().nbt_data,
            ..StackRequestItem::default()
        })
        .unwrap();
        let seed = w.into_inner();
        let decoded = fuzz_round_trip(
            &seed,
            0,
            StackRequestItem::decode,
            |w: &mut Writer, x: &StackRequestItem| x.encode(w),
        );
        assert!(decoded > 0);
        let mut w = Writer::new(Vec::new(), 0);
        MaterialReducer {
            input_item: ItemType::default(),
            outputs: vec![MaterialReducerOutput::default(); 3],
        }
        .encode(&mut w)
        .unwrap();
        let decoded = fuzz_round_trip(
            &w.into_inner(),
            0,
            MaterialReducer::decode,
            |w: &mut Writer, x: &MaterialReducer| x.encode(w),
        );
        assert!(decoded > 0);
    }
}
