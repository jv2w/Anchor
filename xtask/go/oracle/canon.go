package main

import (
	"bytes"
	"fmt"
	"reflect"
	"sort"

	"github.com/go-gl/mathgl/mgl32"
	"github.com/sandertv/gophertunnel/minecraft/nbt"
	"github.com/sandertv/gophertunnel/minecraft/protocol"
)

// canonWriter is gophertunnel's protocol.Writer with every NBT write made canonical (compound keys in byte
// order). Methods that write NBT themselves or marshal nested values that may hold NBT are reimplemented
// here, mirroring writer.go, so the nested values are written through canonWriter too.
type canonWriter struct {
	*protocol.Writer
	out *bytes.Buffer
	// multiKey counts written compounds with more than one key.
	multiKey int
}

func newCanonWriter(out *bytes.Buffer) *canonWriter {
	return &canonWriter{Writer: protocol.NewWriter(out, ShieldID), out: out}
}

func (c *canonWriter) NBT(m *map[string]any, encoding nbt.Encoding) {
	newNBTEncoder(c.out, encoding, &c.multiKey).root(*m)
}

func (c *canonWriter) NBTList(m *[]any, encoding nbt.Encoding) {
	newNBTEncoder(c.out, encoding, &c.multiKey).rootList(*m)
}

// EntityMetadata mirrors Writer.EntityMetadata: keys sorted ascending.
func (c *canonWriter) EntityMetadata(x *protocol.EntityMetadata) {
	l := uint32(len(*x))
	c.Varuint32(&l)
	keys := make([]uint32, 0, l)
	for k := range *x {
		keys = append(keys, k)
	}
	sort.Slice(keys, func(i, j int) bool { return keys[i] < keys[j] })
	for _, key := range keys {
		c.Varuint32(&key)
		writeType := func(dataType uint32) {
			c.Varuint32(&dataType)
			legacy := byte(dataType)
			c.Uint8(&legacy)
		}
		switch v := (*x)[key].(type) {
		case byte:
			writeType(protocol.EntityDataTypeByte)
			c.Uint8(&v)
		case int16:
			writeType(protocol.EntityDataTypeInt16)
			c.Int16(&v)
		case int32:
			writeType(protocol.EntityDataTypeInt32)
			c.Varint32(&v)
		case float32:
			writeType(protocol.EntityDataTypeFloat32)
			c.Float32(&v)
		case string:
			writeType(protocol.EntityDataTypeString)
			c.String(&v)
		case map[string]any:
			writeType(protocol.EntityDataTypeCompoundTag)
			c.NBT(&v, nbt.NetworkLittleEndian)
		case protocol.BlockPos:
			writeType(protocol.EntityDataTypeBlockPos)
			c.BlockPos(&v)
		case int64:
			writeType(protocol.EntityDataTypeInt64)
			c.Varint64(&v)
		case mgl32.Vec3:
			writeType(protocol.EntityDataTypeVec3)
			c.Vec3(&v)
		default:
			c.UnknownEnumOption(reflect.TypeOf(v), "entity metadata")
		}
	}
}

// userData mirrors Writer.itemUserData.
func (c *canonWriter) userData(data map[string]any, placeOn, canBreak []string, tick int64, present, shield bool) {
	if !present {
		var zero uint32
		c.Varuint32(&zero)
		return
	}
	buf := new(bytes.Buffer)
	w := protocol.NewWriter(buf, ShieldID)
	var length int16
	if len(data) != 0 {
		length = -1
		version := uint8(1)
		w.Int16(&length)
		w.Uint8(&version)
		newNBTEncoder(buf, nbt.LittleEndian, &c.multiKey).root(data)
	} else {
		w.Int16(&length)
	}
	protocol.FuncSliceUint32Length(w, &placeOn, w.StringUTF)
	protocol.FuncSliceUint32Length(w, &canBreak, w.StringUTF)
	if shield {
		w.Int64(&tick)
	}
	extra := buf.Bytes()
	c.ByteSlice(&extra)
}

func (c *canonWriter) Item(x *protocol.ItemStack) {
	c.Varint32(&x.NetworkID)
	c.Uint16(&x.Count)
	c.Varuint32(&x.MetadataValue)
	c.Varint32(&x.BlockRuntimeID)
	c.userData(x.NBTData, x.CanBePlacedOn, x.CanBreak, x.BlockingTick, x.NetworkID != 0, x.NetworkID == ShieldID)
}

func (c *canonWriter) ItemInstance(i *protocol.ItemInstance) {
	x := &i.Stack
	id := int16(x.NetworkID)
	c.Int16(&id)
	c.Uint16(&x.Count)
	c.Varuint32(&x.MetadataValue)
	hasNetID := i.StackNetworkID != 0
	c.Bool(&hasNetID)
	if hasNetID {
		c.Varint32(&i.StackNetworkID)
	}
	protocol.IntegerFunc(&x.BlockRuntimeID, c.Varuint32)
	c.userData(x.NBTData, x.CanBePlacedOn, x.CanBreak, x.BlockingTick, x.NetworkID != 0, x.NetworkID == ShieldID)
}

func (c *canonWriter) StackRequestItem(x *protocol.StackRequestItem) {
	hasItem := x.Identifier != ""
	variant := uint32(0)
	if hasItem {
		variant = protocol.ItemDescriptorDefault
	}
	c.Varuint32(&variant)
	legacy := uint8(variant)
	c.Uint8(&legacy)
	if hasItem {
		c.String(&x.Identifier)
		protocol.IntegerFunc(&x.MetadataValue, c.Varint32)
	}
	protocol.IntegerFunc(&x.Count, c.Int16)
	protocol.IntegerFunc(&x.BlockRuntimeID, c.Varuint32)
	c.userData(x.NBTData, x.CanBePlacedOn, x.CanBreak, x.BlockingTick, hasItem, x.Identifier == "minecraft:shield")
}

func (c *canonWriter) StackRequestAction(x *protocol.StackRequestAction) {
	t, ok := tables.actionTag[reflect.TypeOf(*x)]
	if !ok {
		c.UnknownEnumOption(fmt.Sprintf("%T", *x), "stack request action type")
	}
	c.Varuint32(&t.variant)
	c.Uint8(&t.id)
	(*x).Marshal(c)
}

func (c *canonWriter) ShapeData(x *protocol.ShapeData) {
	t, ok := tables.shapeTag[reflect.TypeOf(*x)]
	if !ok {
		c.UnknownEnumOption(fmt.Sprintf("%T", *x), "debug shape data type")
	}
	c.Varuint32(&t)
	(*x).Marshal(c)
}

func (c *canonWriter) PlayerInventoryAction(x *protocol.UseItemTransactionData) {
	playerInventoryAction(c, x)
}

func (c *canonWriter) MaterialReducer(m *protocol.MaterialReducer) {
	mix := (m.InputItem.NetworkID << 16) | int32(m.InputItem.MetadataValue)
	c.Varint32(&mix)
	protocol.Slice(c, &m.Outputs)
}

// playerInventoryAction mirrors Reader/Writer.PlayerInventoryAction for any IO.
func playerInventoryAction(r protocol.IO, x *protocol.UseItemTransactionData) {
	r.Varint32(&x.LegacyRequestID)
	protocol.OptionalFunc(r, &x.LegacySetItemSlots, func(slots *[]protocol.LegacySetItemSlot) {
		protocol.Slice(r, slots)
	})
	protocol.Slice(r, &x.Actions)
	protocol.IntegerFunc(&x.ActionType, r.Varint32)
	protocol.IntegerFunc(&x.TriggerType, r.Uint8)
	r.BlockPos(&x.BlockPosition)
	protocol.IntegerFunc(&x.BlockFace, r.Uint8)
	r.Varint32(&x.HotBarSlot)
	r.Uint8(&x.Hand)
	r.ItemInstance(&x.HeldItem)
	r.Vec3(&x.Position)
	r.Vec3(&x.ClickedPosition)
	r.Varuint32(&x.BlockRuntimeID)
	r.Uint8(&x.ClientPrediction)
	r.Uint8(&x.ClientCooldownState)
}
