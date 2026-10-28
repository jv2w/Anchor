//! The tag frames of the protocol unions, hand-written like gophertunnel's `reader.go` and
//! `writer.go`: the generated union enums give the variant tables (`from_id`, `id`) and the
//! variant bodies, and these methods read and write the tags around them.

use super::io::{Codec, Error, Reader, Result, Writer};
use super::types::{
    Event, InventoryAction, InventoryTransactionData, ItemDescriptor, ItemDescriptorCount,
    LegacySetItemSlot, ShapeData, StackRequestAction, UseItemTransactionData,
    stack_request_action_variant,
};

/// gophertunnel `ItemDescriptorInvalid`.
const DESCRIPTOR_INVALID: u8 = 0;
/// gophertunnel `ItemDescriptorDefault`.
const DESCRIPTOR_DEFAULT: u8 = 1;
/// gophertunnel `ItemDescriptorMoLang`.
const DESCRIPTOR_MOLANG: u8 = 2;
/// gophertunnel `ItemDescriptorItemTag`.
const DESCRIPTOR_ITEM_TAG: u8 = 3;
/// The aux value written after invalid and item tag descriptors.
const DESCRIPTOR_AUX: i32 = 32767;
/// gophertunnel `StackRequestActionPlaceInContainer`: variants from here on skip two IDs.
const ACTION_PLACE_IN_CONTAINER: u32 = 7;
/// gophertunnel `StackRequestActionCraftResultsDeprecated`, the last action.
const ACTION_LAST: u8 = 20;

/// A value that does not fit the narrower wire integer gophertunnel truncates it to.
fn out_of_range(field: &'static str) -> Error {
    Error::InvalidValue {
        field,
        reason: "out of range for its wire type",
    }
}

impl Reader<'_> {
    /// `StackRequestAction`: the variant index, the legacy ID (not checked, like
    /// gophertunnel) and the action.
    pub(crate) fn stack_request_action(&mut self) -> Result<StackRequestAction> {
        let variant = self.varuint32()?;
        self.uint8()?;
        let unknown = |value: u32, name| Error::UnknownEnum {
            name,
            value: value.into(),
        };
        if variant > stack_request_action_variant(ACTION_LAST) {
            return Err(unknown(variant, "StackRequestAction variant"));
        }
        let skip = if variant >= ACTION_PLACE_IN_CONTAINER {
            2
        } else {
            0
        };
        let id = u8::try_from(variant + skip)
            .map_err(|_| unknown(variant, "StackRequestAction variant"))?;
        StackRequestAction::from_id(id)
            .ok_or_else(|| unknown(id.into(), "StackRequestAction ID"))?
            .decode_variant(self)
    }

    /// `EventType`: the event with default fields; [`Self::event_ordinal`] and the event body
    /// follow later in the packet.
    pub(crate) fn event_type(&mut self) -> Result<Event> {
        let t = self.varint32()?;
        Event::from_id(t).ok_or(Error::UnknownEnum {
            name: "Event type",
            value: t.into(),
        })
    }

    /// `EventOrdinal`: fails for an event without an ordinal; the ordinal read is not checked,
    /// like gophertunnel.
    pub(crate) fn event_ordinal(&mut self, x: &Event) -> Result<()> {
        if x.ordinal().is_none() {
            return Err(Error::UnknownEnum {
                name: "Event ordinal",
                value: x.id().into(),
            });
        }
        self.varuint32()?;
        Ok(())
    }

    /// `TransactionDataType`: the transaction data with default fields; its body follows later
    /// in the packet.
    pub(crate) fn transaction_data_type(&mut self) -> Result<InventoryTransactionData> {
        let t = self.varuint32()?;
        InventoryTransactionData::from_id(t).ok_or(Error::UnknownEnum {
            name: "InventoryTransactionData type",
            value: t.into(),
        })
    }

    /// `ShapeData`: the shape type and the shape.
    pub(crate) fn shape_data(&mut self) -> Result<ShapeData> {
        let t = self.varuint32()?;
        ShapeData::from_id(t)
            .ok_or(Error::UnknownEnum {
                name: "ShapeData type",
                value: t.into(),
            })?
            .decode_variant(self)
    }

    /// `ItemDescriptorCount`: a descriptor in the recipe form (variant, type name, body, aux
    /// values that are not checked, like gophertunnel) and its count.
    pub(crate) fn item_descriptor_count(&mut self) -> Result<ItemDescriptorCount> {
        let variant = self.varuint32()?;
        let descriptor = if variant == u32::from(DESCRIPTOR_INVALID) {
            self.varint32()?;
            ItemDescriptor::from_id(DESCRIPTOR_INVALID).unwrap_or_default()
        } else {
            if variant != u32::from(DESCRIPTOR_DEFAULT) {
                return Err(Error::UnknownEnum {
                    name: "ItemDescriptor variant",
                    value: variant.into(),
                });
            }
            let name = self.string()?;
            let descriptor = [DESCRIPTOR_DEFAULT, DESCRIPTOR_MOLANG, DESCRIPTOR_ITEM_TAG]
                .into_iter()
                .filter_map(ItemDescriptor::from_id)
                .find(|d| d.type_name() == name)
                .ok_or(Error::InvalidValue {
                    field: "ItemDescriptor name",
                    reason: "unknown name",
                })?
                .decode_variant(self)?;
            if descriptor.id() == DESCRIPTOR_ITEM_TAG {
                self.varint32()?;
            }
            descriptor
        };
        Ok(ItemDescriptorCount {
            descriptor,
            count: self.varint32()?,
        })
    }

    /// `StackRequestItemDescriptorCount`: a descriptor in the stack request form (variant,
    /// legacy ID not checked like gophertunnel, body) and its count as a uint16.
    pub(crate) fn stack_request_item_descriptor_count(&mut self) -> Result<ItemDescriptorCount> {
        let variant = self.varuint32()?;
        self.uint8()?;
        let descriptor = u8::try_from(variant)
            .ok()
            .and_then(ItemDescriptor::from_id)
            .ok_or(Error::UnknownEnum {
                name: "ItemDescriptor ID",
                value: variant.into(),
            })?
            .decode_variant(self)?;
        Ok(ItemDescriptorCount {
            descriptor,
            count: self.uint16()?.into(),
        })
    }

    /// `PlayerInventoryAction`: the legacy fields and actions of a use-item transaction, then
    /// the transaction data.
    pub(crate) fn player_inventory_action(&mut self) -> Result<UseItemTransactionData> {
        let legacy_request_id = self.varint32()?;
        let legacy_set_item_slots = if self.bool()? {
            Some(self.list_varuint32(LegacySetItemSlot::decode)?)
        } else {
            None
        };
        Ok(UseItemTransactionData {
            legacy_request_id,
            legacy_set_item_slots,
            actions: self.list_varuint32(InventoryAction::decode)?,
            action_type: self.varint32()?.cast_unsigned(),
            trigger_type: self.uint8()?.into(),
            block_position: self.block_pos()?,
            block_face: self.uint8()?.into(),
            hot_bar_slot: self.varint32()?,
            hand: self.uint8()?,
            held_item: self.item_instance()?,
            position: self.vec3()?,
            clicked_position: self.vec3()?,
            block_runtime_id: self.varuint32()?,
            client_prediction: self.uint8()?,
            client_cooldown_state: self.uint8()?,
        })
    }
}

impl Writer {
    /// `StackRequestAction`
    pub(crate) fn stack_request_action(&mut self, x: &StackRequestAction) -> Result<()> {
        let id = x.id();
        self.varuint32(stack_request_action_variant(id));
        self.uint8(id);
        x.encode_variant(self)
    }

    /// `EventType`
    pub(crate) fn event_type(&mut self, x: &Event) {
        self.varint32(x.id());
    }

    /// `EventOrdinal`: fails for an event without an ordinal.
    pub(crate) fn event_ordinal(&mut self, x: &Event) -> Result<()> {
        let ordinal = x.ordinal().ok_or(Error::UnknownEnum {
            name: "Event ordinal",
            value: x.id().into(),
        })?;
        self.varuint32(ordinal);
        Ok(())
    }

    /// `TransactionDataType`
    pub(crate) fn transaction_data_type(&mut self, x: &InventoryTransactionData) {
        self.varuint32(x.id());
    }

    /// `ShapeData`
    pub(crate) fn shape_data(&mut self, x: &ShapeData) -> Result<()> {
        self.varuint32(x.id());
        x.encode_variant(self)
    }

    /// `ItemDescriptorCount`
    pub(crate) fn item_descriptor_count(&mut self, x: &ItemDescriptorCount) -> Result<()> {
        let id = x.descriptor.id();
        self.varuint32(id.min(DESCRIPTOR_DEFAULT).into());
        if id == DESCRIPTOR_INVALID {
            self.varint32(DESCRIPTOR_AUX);
        } else {
            self.string(x.descriptor.type_name())?;
            x.descriptor.encode_variant(self)?;
            if id == DESCRIPTOR_ITEM_TAG {
                self.varint32(DESCRIPTOR_AUX);
            }
        }
        self.varint32(x.count);
        Ok(())
    }

    /// `StackRequestItemDescriptorCount`: fails if the count does not fit a uint16 (which
    /// gophertunnel truncates).
    pub(crate) fn stack_request_item_descriptor_count(
        &mut self,
        x: &ItemDescriptorCount,
    ) -> Result<()> {
        let id = x.descriptor.id();
        self.varuint32(id.into());
        self.uint8(id);
        x.descriptor.encode_variant(self)?;
        let count = u16::try_from(x.count).map_err(|_| out_of_range("item descriptor count"))?;
        self.uint16(count);
        Ok(())
    }

    /// `PlayerInventoryAction`: fails if the trigger type or block face does not fit a uint8
    /// (which gophertunnel truncates).
    pub(crate) fn player_inventory_action(&mut self, x: &UseItemTransactionData) -> Result<()> {
        self.varint32(x.legacy_request_id);
        self.bool(x.legacy_set_item_slots.is_some());
        if let Some(slots) = &x.legacy_set_item_slots {
            self.varuint32(super::length(slots.len())?);
            for slot in slots {
                slot.encode(self)?;
            }
        }
        self.varuint32(super::length(x.actions.len())?);
        for action in &x.actions {
            action.encode(self)?;
        }
        self.varint32(x.action_type.cast_signed());
        self.uint8(u8::try_from(x.trigger_type).map_err(|_| out_of_range("trigger type"))?);
        self.block_pos(x.block_position);
        self.uint8(u8::try_from(x.block_face).map_err(|_| out_of_range("block face"))?);
        self.varint32(x.hot_bar_slot);
        self.uint8(x.hand);
        self.item_instance(&x.held_item)?;
        self.vec3(x.position);
        self.vec3(x.clicked_position);
        self.varuint32(x.block_runtime_id);
        self.uint8(x.client_prediction);
        self.uint8(x.client_cooldown_state);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::fuzz;
    use crate::protocol::io::tests::{reader, writer};

    fn bytes(f: impl FnOnce(&mut Writer) -> Result<()>) -> Vec<u8> {
        let mut w = writer();
        f(&mut w).unwrap();
        w.into_inner()
    }

    #[test]
    fn every_stack_request_action_round_trips_with_its_variant_index() {
        for id in 0..=ACTION_LAST {
            let Some(action) = StackRequestAction::from_id(id) else {
                continue;
            };
            let b = bytes(|w| w.stack_request_action(&action));
            let variant = if id > 8 { id - 2 } else { id };
            assert_eq!(b[..2], [variant, id]);
            let mut r = reader(&b);
            assert_eq!(r.stack_request_action(), Ok(action));
            assert!(r.remaining().is_empty());
        }
        let mut r = reader(&[19, 0]);
        assert!(matches!(
            r.stack_request_action(),
            Err(Error::UnknownEnum { .. })
        ));
    }

    #[test]
    fn the_reserved_craft_and_the_last_action_decode_and_the_next_is_rejected() {
        // Variant 16 is the reserved craft (ID 18), variant 18 the deprecated craft results
        // (ID 20), the last action gophertunnel reads.
        for (variant, id) in [(16, 18), (18, 20)] {
            let action = StackRequestAction::from_id(id).unwrap();
            let b = bytes(|w| w.stack_request_action(&action));
            assert_eq!(b[..2], [variant, id]);
            let mut r = reader(&b);
            assert_eq!(r.stack_request_action(), Ok(action));
            assert!(r.remaining().is_empty());
        }
        assert!(matches!(
            reader(&[19, 21]).stack_request_action(),
            Err(Error::UnknownEnum { .. })
        ));
    }

    #[test]
    fn shapes_and_transaction_data_round_trip() {
        for id in 0..16 {
            if let Some(shape) = ShapeData::from_id(id) {
                let b = bytes(|w| w.shape_data(&shape));
                assert_eq!(reader(&b).shape_data(), Ok(shape));
            }
            if let Some(data) = InventoryTransactionData::from_id(id) {
                let b = bytes(|w| {
                    w.transaction_data_type(&data);
                    Ok(())
                });
                assert_eq!(reader(&b).transaction_data_type(), Ok(data));
            }
        }
        assert!(reader(&[99]).shape_data().is_err());
    }

    #[test]
    fn events_without_an_ordinal_fail_both_ways() {
        let mut checked = 0;
        for id in 0..64 {
            let Some(event) = Event::from_id(id) else {
                continue;
            };
            let b = bytes(|w| {
                w.event_type(&event);
                Ok(())
            });
            assert_eq!(reader(&b).event_type(), Ok(event.clone()));
            if let Some(ordinal) = event.ordinal() {
                let b = bytes(|w| w.event_ordinal(&event));
                assert_eq!(b, [u8::try_from(ordinal).unwrap()]);
                assert_eq!(reader(&b).event_ordinal(&event), Ok(()));
            } else {
                assert!(writer().event_ordinal(&event).is_err());
                assert!(reader(&[0]).event_ordinal(&event).is_err());
                checked += 1;
            }
        }
        assert!(checked > 0);
    }

    #[test]
    fn item_descriptor_counts_round_trip_in_both_forms() {
        for id in 0..=DESCRIPTOR_ITEM_TAG {
            let x = ItemDescriptorCount {
                descriptor: ItemDescriptor::from_id(id).unwrap(),
                count: 7,
            };
            let b = bytes(|w| w.item_descriptor_count(&x));
            let mut r = reader(&b);
            assert_eq!(r.item_descriptor_count(), Ok(x.clone()));
            assert!(r.remaining().is_empty());
            let b = bytes(|w| w.stack_request_item_descriptor_count(&x));
            let mut r = reader(&b);
            assert_eq!(r.stack_request_item_descriptor_count(), Ok(x));
            assert!(r.remaining().is_empty());
        }
        // A type name other than the known ones.
        let b = [1, 1, b'x'];
        assert!(reader(&b).item_descriptor_count().is_err());
        let x = ItemDescriptorCount {
            count: 70_000,
            ..ItemDescriptorCount::default()
        };
        assert!(writer().stack_request_item_descriptor_count(&x).is_err());
    }

    #[test]
    fn use_item_data_rejects_values_gophertunnel_would_truncate() {
        let x = UseItemTransactionData {
            action_type: u32::MAX,
            trigger_type: 2,
            block_face: 5,
            legacy_set_item_slots: Some(Vec::new()),
            ..UseItemTransactionData::default()
        };
        let b = bytes(|w| w.player_inventory_action(&x));
        let mut r = reader(&b);
        assert_eq!(r.player_inventory_action(), Ok(x.clone()));
        assert!(r.remaining().is_empty());
        let wide = UseItemTransactionData {
            trigger_type: 256,
            ..x.clone()
        };
        assert!(writer().player_inventory_action(&wide).is_err());
        let negative = UseItemTransactionData {
            block_face: -1,
            ..x
        };
        assert!(writer().player_inventory_action(&negative).is_err());
    }

    #[test]
    fn mutated_frames_never_panic() {
        let action = StackRequestAction::from_id(12).unwrap();
        let seed = bytes(|w| {
            w.stack_request_action(&action)?;
            w.item_descriptor_count(&ItemDescriptorCount {
                descriptor: ItemDescriptor::from_id(DESCRIPTOR_ITEM_TAG).unwrap(),
                count: 3,
            })?;
            w.player_inventory_action(&UseItemTransactionData::default())
        });
        let decoded = fuzz(&seed, 5000, |data| {
            let mut r = reader(data);
            r.stack_request_action()?;
            r.item_descriptor_count()?;
            r.player_inventory_action()?;
            Ok(())
        });
        assert!(decoded > 0);
    }
}
