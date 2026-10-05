package main

import (
	"bytes"
	"encoding/binary"
	"fmt"
	"math"
	"math/rand/v2"
	"reflect"
	"sort"

	"github.com/sandertv/gophertunnel/minecraft/nbt"
)

// NBT tag type IDs.
const (
	tagEnd       = 0
	tagByte      = 1
	tagShort     = 2
	tagInt       = 3
	tagLong      = 4
	tagFloat     = 5
	tagDouble    = 6
	tagByteArray = 7
	tagString    = 8
	tagList      = 9
	tagCompound  = 10
	tagIntArray  = 11
	tagLongArray = 12
)

// nbtEncoder writes NBT canonically: compound keys in byte order. It accepts exactly the Go value shapes
// that gophertunnel's NBT decoder produces when decoding into map[string]any / []any.
type nbtEncoder struct {
	w       *bytes.Buffer
	network bool
	// multiKey counts compounds with more than one key (gophertunnel writes those in random order).
	multiKey *int
}

func newNBTEncoder(w *bytes.Buffer, encoding nbt.Encoding, multiKey *int) nbtEncoder {
	switch encoding {
	case nbt.NetworkLittleEndian:
		return nbtEncoder{w: w, network: true, multiKey: multiKey}
	case nbt.LittleEndian:
		return nbtEncoder{w: w, network: false, multiKey: multiKey}
	default:
		panic(fmt.Sprintf("unsupported nbt encoding %T", encoding))
	}
}

// root writes a root compound with an empty name.
func (e nbtEncoder) root(m map[string]any) {
	e.w.WriteByte(tagCompound)
	e.str("")
	e.payload(m)
}

// rootList writes a root list with an empty name.
func (e nbtEncoder) rootList(l []any) {
	e.w.WriteByte(tagList)
	e.str("")
	e.payload(l)
}

func (e nbtEncoder) i16(v int16) {
	e.w.Write(binary.LittleEndian.AppendUint16(nil, uint16(v)))
}

func (e nbtEncoder) i32(v int32) {
	if e.network {
		e.w.Write(binary.AppendUvarint(nil, uint64(uint32(v<<1)^uint32(v>>31))))
		return
	}
	e.w.Write(binary.LittleEndian.AppendUint32(nil, uint32(v)))
}

func (e nbtEncoder) i64(v int64) {
	if e.network {
		e.w.Write(binary.AppendUvarint(nil, uint64(v<<1)^uint64(v>>63)))
		return
	}
	e.w.Write(binary.LittleEndian.AppendUint64(nil, uint64(v)))
}

func (e nbtEncoder) str(s string) {
	if e.network {
		e.w.Write(binary.AppendUvarint(nil, uint64(len(s))))
	} else {
		e.i16(int16(uint16(len(s))))
	}
	e.w.WriteString(s)
}

func nbtTag(v any) byte {
	switch v.(type) {
	case uint8:
		return tagByte
	case int16:
		return tagShort
	case int32:
		return tagInt
	case int64:
		return tagLong
	case float32:
		return tagFloat
	case float64:
		return tagDouble
	case string:
		return tagString
	case map[string]any:
		return tagCompound
	case []any, []byte, []int32, []int64:
		return tagList
	}
	t := reflect.TypeOf(v)
	if t != nil && t.Kind() == reflect.Array {
		switch t.Elem().Kind() {
		case reflect.Uint8:
			return tagByteArray
		case reflect.Int32:
			return tagIntArray
		case reflect.Int64:
			return tagLongArray
		}
	}
	panic(fmt.Sprintf("unsupported nbt value %T", v))
}

func (e nbtEncoder) payload(v any) {
	switch v := v.(type) {
	case uint8:
		e.w.WriteByte(v)
	case int16:
		e.i16(v)
	case int32:
		e.i32(v)
	case int64:
		e.i64(v)
	case float32:
		e.w.Write(binary.LittleEndian.AppendUint32(nil, math.Float32bits(v)))
	case float64:
		e.w.Write(binary.LittleEndian.AppendUint64(nil, math.Float64bits(v)))
	case string:
		e.str(v)
	case map[string]any:
		keys := make([]string, 0, len(v))
		for k := range v {
			keys = append(keys, k)
		}
		sort.Strings(keys)
		if len(keys) > 1 && e.multiKey != nil {
			*e.multiKey++
		}
		for _, k := range keys {
			e.w.WriteByte(nbtTag(v[k]))
			e.str(k)
			e.payload(v[k])
		}
		e.w.WriteByte(tagEnd)
	case []any:
		elem := byte(tagEnd)
		if len(v) > 0 {
			elem = nbtTag(v[0])
		}
		e.w.WriteByte(elem)
		e.i32(int32(len(v)))
		for _, x := range v {
			e.payload(x)
		}
	case []byte:
		e.w.WriteByte(tagByte)
		e.i32(int32(len(v)))
		e.w.Write(v)
	case []int32:
		e.w.WriteByte(tagInt)
		e.i32(int32(len(v)))
		for _, x := range v {
			e.i32(x)
		}
	case []int64:
		e.w.WriteByte(tagLong)
		e.i32(int32(len(v)))
		for _, x := range v {
			e.i64(x)
		}
	default:
		a := reflect.ValueOf(v)
		e.i32(int32(a.Len()))
		for i := 0; i < a.Len(); i++ {
			switch nbtTag(v) {
			case tagByteArray:
				e.w.WriteByte(uint8(a.Index(i).Uint()))
			case tagIntArray:
				e.i32(int32(a.Index(i).Int()))
			default:
				e.i64(a.Index(i).Int())
			}
		}
	}
}

// rootKeys is the most keys of a compound in an NBT root, which nbtEncoder writes.
const rootKeys = 4

// nbtGen generates small random NBT trees that gophertunnel decodes and re-encodes without loss:
//   - no empty list typed other than TAG_End, TAG_Int or TAG_Long (gophertunnel decodes them to []any{} and
//     re-encodes them as TAG_End lists),
//   - no NaN floats, no duplicate keys, root name always empty, depth <= 3, <= 4 elements per list,
//     <= keys keys per compound.
type nbtGen struct {
	rng *rand.Rand
	// keys is the most keys of a compound: 1 for NBT that gophertunnel's encoder writes, which writes
	// the keys of a Go map in random order.
	keys int
}

func (g nbtGen) compound(depth int) map[string]any {
	n := g.rng.IntN(g.keys + 1)
	m := make(map[string]any, n)
	for len(m) < n {
		m[randString(g.rng, 8)] = g.value(depth)
	}
	return m
}

// value returns a random NBT value. depth counts enclosing compounds and lists.
func (g nbtGen) value(depth int) any {
	kinds := 10
	if depth < 3 {
		kinds = 12
	}
	switch g.rng.IntN(kinds) {
	case 0:
		return uint8(g.rng.Uint32())
	case 1:
		return int16(randBits(g.rng, 16))
	case 2:
		return int32(randBits(g.rng, 32))
	case 3:
		return int64(randBits(g.rng, 64))
	case 4:
		return randFloat32(g.rng)
	case 5:
		return randFloat64(g.rng)
	case 6:
		return randString(g.rng, 12)
	case 7:
		return g.array(tagByteArray)
	case 8:
		return g.array(tagIntArray)
	case 9:
		return g.array(tagLongArray)
	case 10:
		return g.list(depth + 1)
	default:
		return g.compound(depth + 1)
	}
}

func (g nbtGen) array(tag byte) any {
	n := g.rng.IntN(5)
	var elem reflect.Type
	switch tag {
	case tagByteArray:
		elem = reflect.TypeOf(uint8(0))
	case tagIntArray:
		elem = reflect.TypeOf(int32(0))
	default:
		elem = reflect.TypeOf(int64(0))
	}
	a := reflect.New(reflect.ArrayOf(n, elem)).Elem()
	for i := 0; i < n; i++ {
		switch tag {
		case tagByteArray:
			a.Index(i).SetUint(uint64(uint8(g.rng.Uint32())))
		case tagIntArray:
			a.Index(i).SetInt(int64(int32(randBits(g.rng, 32))))
		default:
			a.Index(i).SetInt(int64(randBits(g.rng, 64)))
		}
	}
	return a.Interface()
}

func (g nbtGen) list(depth int) any {
	n := g.rng.IntN(5)
	if n == 0 {
		switch g.rng.IntN(3) {
		case 0:
			return []int32{}
		case 1:
			return []int64{}
		default:
			return []any{}
		}
	}
	// Pick the element shape from one generated sample, then generate the rest with the same tag.
	first := g.value(depth)
	tag := nbtTag(first)
	switch tag {
	case tagByte:
		b := make([]byte, n)
		for i := range b {
			b[i] = uint8(g.rng.Uint32())
		}
		return b
	case tagInt:
		l := make([]int32, n)
		for i := range l {
			l[i] = int32(randBits(g.rng, 32))
		}
		return l
	case tagLong:
		l := make([]int64, n)
		for i := range l {
			l[i] = int64(randBits(g.rng, 64))
		}
		return l
	}
	l := make([]any, n)
	l[0] = first
	for i := 1; i < n; i++ {
		for {
			v := g.value(depth)
			if nbtTag(v) == tag {
				l[i] = v
				break
			}
		}
	}
	return l
}

// rootList returns a list that gophertunnel can decode into []any at the root: elements must not be
// TAG_Byte, TAG_Int or TAG_Long (those decode into typed slices that cannot be stored in a []any).
func (g nbtGen) rootList() []any {
	n := g.rng.IntN(4)
	l := make([]any, n)
	for i := range l {
		l[i] = g.compound(1)
	}
	return l
}
