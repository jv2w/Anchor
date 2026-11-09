package main

import (
	"bytes"
	"image/color"
	"math"
	"math/rand/v2"
	"reflect"
	"runtime"
	"strings"
	"unicode/utf8"

	"github.com/go-gl/mathgl/mgl32"
	"github.com/google/uuid"
	"github.com/sandertv/gophertunnel/minecraft/nbt"
	"github.com/sandertv/gophertunnel/minecraft/protocol"
)

// siteKey identifies an IO call site by the return addresses of its innermost callers.
type siteKey [6]uintptr

// Learned call-site modes.
const (
	modeRandom = iota
	// modeLength: the site writes a slice length; draw 0..4.
	modeLength
	// modeKeep: values from this site were rejected before; keep half the time the value already in the pointer
	// (what a Writer would write for a zero/derived field).
	modeKeep
)

// reject aborts a sample attempt.
type reject struct{ reason string }

// call records where an IO call started writing.
type call struct {
	start int
	site  siteKey
	// val is the integer (masked to bits) or string the call drew, if any.
	val  any
	bits uint
}

// randomIO implements protocol.IO. Every call picks a random valid value, stores it in the pointer and
// writes it with gophertunnel's Writer, so the output follows gophertunnel's exact wire format. NBT roots
// are written by nbtEncoder (compound keys in byte order); the NBT nested in other values (item user
// data, entity metadata), which gophertunnel's Writer writes itself, has at most one key per compound,
// so every sample has one encoding.
type randomIO struct {
	w     *protocol.Writer
	out   *bytes.Buffer
	rng   *rand.Rand
	modes map[siteKey]uint8
	calls []call
	// keepProb is the chance that any site keeps the pointer's current value. It rises with the attempt
	// count so constrained packets fall back to mostly-default values.
	keepProb float64
	// dims holds the last two image dimensions drawn in a skin, and dimsCall the call index of the last.
	dims     [2]uint32
	dimsCall int
	names    map[uintptr]string
	// sample is the index of the sample being generated. EventType takes the event at this index, so the
	// Event samples hold every event, not only those a random draw picks.
	sample int
}

// inSkinImage reports whether the last recorded call came from Skin or SkinAnimation Marshal. Their
// Reader validates width*height*4 == len(data), which random values never satisfy.
func (r *randomIO) inSkinImage() bool {
	name := r.lastCaller()
	return strings.HasSuffix(name, "protocol.(*Skin).Marshal") || strings.HasSuffix(name, "protocol.(*SkinAnimation).Marshal")
}

// inUpdateTrade reports whether the last recorded call came from UpdateTrade Marshal, whose raw
// SerialisedOffers Anchor reads as one network little-endian NBT root.
func (r *randomIO) inUpdateTrade() bool {
	return strings.HasSuffix(r.lastCaller(), "packet.(*UpdateTrade).Marshal")
}

// lastCaller is the function name of the last recorded call's caller.
func (r *randomIO) lastCaller() string {
	pc := r.calls[len(r.calls)-1].site[0]
	name, ok := r.names[pc]
	if !ok {
		if f := runtime.FuncForPC(pc - 1); f != nil {
			name = f.Name()
		}
		r.names[pc] = name
	}
	return name
}

func newRandomIO(rng *rand.Rand) *randomIO {
	out := new(bytes.Buffer)
	return &randomIO{w: protocol.NewWriter(out, ShieldID), out: out, rng: rng, modes: map[siteKey]uint8{}, names: map[uintptr]string{}}
}

func (r *randomIO) reset(keepProb float64) {
	r.out.Reset()
	r.calls = r.calls[:0]
	r.dimsCall = -2
	r.keepProb = keepProb
}

// begin records the call site of the IO method that calls it and returns the site's mode.
func (r *randomIO) begin() uint8 {
	var pcs [6]uintptr
	runtime.Callers(3, pcs[:])
	k := siteKey(pcs)
	r.calls = append(r.calls, call{start: r.out.Len(), site: k})
	return r.modes[k]
}

// keep reports whether the current call keeps the pointer's value.
func (r *randomIO) keep(mode uint8) bool {
	if mode == modeKeep {
		return r.rng.Float64() < 0.5
	}
	return r.keepProb > 0 && r.rng.Float64() < r.keepProb
}

// learnAt marks the site of the last call that started at or before offset as modeKeep.
func (r *randomIO) learnAt(offset int) {
	for i := len(r.calls) - 1; i >= 0; i-- {
		if r.calls[i].start <= offset {
			if r.modes[r.calls[i].site] == modeRandom {
				r.modes[r.calls[i].site] = modeKeep
			}
			return
		}
	}
}

// learnValue marks as modeKeep the latest of the last few calls that drew value (the value an
// InvalidValue or UnknownEnumOption reported), or the last call if none did.
func (r *randomIO) learnValue(value any) {
	v := reflect.ValueOf(value)
	for i := len(r.calls) - 1; i >= 0 && i >= len(r.calls)-8; i-- {
		c := r.calls[i]
		match := false
		switch cv := c.val.(type) {
		case string:
			match = v.Kind() == reflect.String && v.String() == cv
		case uint64:
			var bits uint64
			switch {
			case v.CanInt():
				bits = uint64(v.Int())
			case v.CanUint():
				bits = v.Uint()
			default:
				continue
			}
			if c.bits < 64 {
				bits &= 1<<c.bits - 1
			}
			match = bits == cv
		}
		if match {
			if r.modes[c.site] == modeRandom {
				r.modes[c.site] = modeKeep
			}
			return
		}
	}
	r.learnLast(modeKeep)
}

func (r *randomIO) learnLast(mode uint8) {
	if n := len(r.calls); n > 0 {
		site := r.calls[n-1].site
		if mode == modeLength || r.modes[site] == modeRandom {
			r.modes[site] = mode
		}
	}
}

// num draws an integer of the given bit width (two's complement for signed) or keeps cur. mode comes from
// begin, called by the IO method itself so the recorded site is the method's caller.
func (r *randomIO) num(mode uint8, cur uint64, bits uint, signed bool) uint64 {
	v := r.draw(mode, cur, bits, signed)
	if bits < 64 {
		v &= 1<<bits - 1
	}
	c := &r.calls[len(r.calls)-1]
	c.val, c.bits = v, bits
	return v
}

func (r *randomIO) draw(mode uint8, cur uint64, bits uint, signed bool) uint64 {
	if mode == modeLength {
		return r.rng.Uint64N(5)
	}
	if r.keep(mode) {
		return cur
	}
	var v uint64
	switch p := r.rng.IntN(100); {
	case p < 35:
		v = r.rng.Uint64N(5)
	case p < 55:
		v = r.rng.Uint64N(256)
	case p < 65 && signed:
		v = uint64(-int64(1 + r.rng.Uint64N(128)))
	default:
		v = randBits(r.rng, bits)
		if signed && r.rng.IntN(2) == 0 {
			v = -v
		}
	}
	return v
}

// randBits returns a random value with a random bit length in 1..bits, so varints of every size occur.
func randBits(rng *rand.Rand, bits uint) uint64 {
	n := 1 + rng.UintN(bits)
	v := rng.Uint64()
	if n < 64 {
		v &= 1<<n - 1
	}
	return v
}

func randFloat32(rng *rand.Rand) float32 {
	switch rng.IntN(5) {
	case 0:
		return 0
	case 1:
		return float32(rng.IntN(201) - 100)
	case 2:
		return (rng.Float32() - 0.5) * 2000
	default:
		for {
			f := math.Float32frombits(rng.Uint32())
			if !math.IsNaN(float64(f)) && !math.IsInf(float64(f), 0) {
				return f
			}
		}
	}
}

func randFloat64(rng *rand.Rand) float64 {
	switch rng.IntN(5) {
	case 0:
		return 0
	case 1:
		return float64(rng.IntN(201) - 100)
	case 2:
		return (rng.Float64() - 0.5) * 2000
	default:
		for {
			f := math.Float64frombits(rng.Uint64())
			if !math.IsNaN(f) && !math.IsInf(f, 0) {
				return f
			}
		}
	}
}

// randString returns random valid UTF-8 of 0..maxRunes runes, mostly ASCII with 2, 3 and 4 byte runes.
func randString(rng *rand.Rand, maxRunes int) string {
	n := rng.IntN(maxRunes + 1)
	b := make([]byte, 0, n*2)
	for i := 0; i < n; i++ {
		var c rune
		switch p := rng.IntN(10); {
		case p < 7:
			c = rune(0x20 + rng.IntN(0x5f))
		case p < 8:
			c = rune(0x80 + rng.IntN(0x780))
		case p < 9:
			c = rune(0x800 + rng.IntN(0xf800))
			if c >= 0xd800 && c < 0xe000 {
				c = 'x'
			}
		default:
			c = rune(0x10000 + rng.IntN(0x100000))
		}
		b = utf8.AppendRune(b, c)
	}
	return string(b)
}

func (r *randomIO) randBytes() []byte {
	b := make([]byte, r.rng.IntN(17))
	for i := range b {
		b[i] = byte(r.rng.Uint32())
	}
	return b
}

// readBack decodes the bytes written since start with gophertunnel's Reader into the pointer, so the
// pointer holds exactly what a Reader would see for the following Marshal logic.
func (r *randomIO) readBack(start int, read func(rd *protocol.Reader)) {
	rd := protocol.NewReader(bytes.NewReader(r.out.Bytes()[start:]), ShieldID, true)
	read(rd)
}

// Primitive methods.

func (r *randomIO) Uint8(x *uint8) {
	*x = uint8(r.num(r.begin(), uint64(*x), 8, false))
	r.w.Uint8(x)
}

func (r *randomIO) Int8(x *int8) {
	*x = int8(r.num(r.begin(), uint64(*x), 8, true))
	r.w.Int8(x)
}

func (r *randomIO) Uint16(x *uint16) {
	*x = uint16(r.num(r.begin(), uint64(*x), 16, false))
	r.w.Uint16(x)
}

func (r *randomIO) Int16(x *int16) {
	*x = int16(r.num(r.begin(), uint64(*x), 16, true))
	r.w.Int16(x)
}

func (r *randomIO) Uint32(x *uint32) {
	*x = uint32(r.num(r.begin(), uint64(*x), 32, false))
	if r.inSkinImage() {
		// Image dimensions: small, so the following image data can have width*height*4 bytes.
		*x = uint32(r.rng.IntN(3))
		r.dims[0], r.dims[1] = r.dims[1], *x
		r.dimsCall = len(r.calls) - 1
	}
	r.w.Uint32(x)
}

func (r *randomIO) Int32(x *int32) {
	*x = int32(r.num(r.begin(), uint64(*x), 32, true))
	r.w.Int32(x)
}

func (r *randomIO) BEInt32(x *int32) {
	*x = int32(r.num(r.begin(), uint64(*x), 32, true))
	r.w.BEInt32(x)
}

func (r *randomIO) Uint64(x *uint64) {
	*x = r.num(r.begin(), *x, 64, false)
	r.w.Uint64(x)
}

func (r *randomIO) Int64(x *int64) {
	*x = int64(r.num(r.begin(), uint64(*x), 64, true))
	r.w.Int64(x)
}

func (r *randomIO) Varint32(x *int32) {
	*x = int32(r.num(r.begin(), uint64(*x), 32, true))
	r.w.Varint32(x)
}

func (r *randomIO) Varuint32(x *uint32) {
	*x = uint32(r.num(r.begin(), uint64(*x), 32, false))
	r.w.Varuint32(x)
}

func (r *randomIO) Varint64(x *int64) {
	*x = int64(r.num(r.begin(), uint64(*x), 64, true))
	r.w.Varint64(x)
}

func (r *randomIO) Varuint64(x *uint64) {
	*x = r.num(r.begin(), *x, 64, false)
	r.w.Varuint64(x)
}

func (r *randomIO) ActorRuntimeID(x *uint64)          { r.Varuint64(x) }
func (r *randomIO) ActorRuntimeIDVarint64(x *int64)   { r.Varint64(x) }
func (r *randomIO) ActorRuntimeIDVaruint32(x *uint32) { r.Varuint32(x) }
func (r *randomIO) ActorUniqueID(x *int64)            { r.Varint64(x) }
func (r *randomIO) ActorUniqueIDInt64(x *int64)       { r.Int64(x) }
func (r *randomIO) ActorUniqueIDUint64(x *uint64)     { r.Uint64(x) }
func (r *randomIO) ActorUniqueIDVaruint64(x *uint64)  { r.Varuint64(x) }

func (r *randomIO) Float32(x *float32) {
	if !r.keep(r.begin()) {
		*x = randFloat32(r.rng)
	}
	r.w.Float32(x)
}

func (r *randomIO) Float64(x *float64) {
	if !r.keep(r.begin()) {
		*x = randFloat64(r.rng)
	}
	r.w.Float64(x)
}

func (r *randomIO) Bool(x *bool) {
	if !r.keep(r.begin()) {
		*x = r.rng.IntN(2) == 0
	}
	r.w.Bool(x)
}

// str keeps only non-empty current strings: an empty string is rarely the value a check wants.
// A string site in modeKeep always keeps: rejected strings are enum names that random text never hits.
func (r *randomIO) str(x *string) {
	mode := r.begin()
	if *x == "" || (mode != modeKeep && !r.keep(mode)) {
		*x = randString(r.rng, 12)
	}
	r.calls[len(r.calls)-1].val = *x
}

func (r *randomIO) String(x *string) {
	r.str(x)
	r.w.String(x)
}

func (r *randomIO) StringUTF(x *string) {
	r.str(x)
	r.w.StringUTF(x)
}

func (r *randomIO) ByteSlice(x *[]byte) {
	if !r.keep(r.begin()) {
		*x = r.randBytes()
	}
	if r.inSkinImage() && r.dimsCall == len(r.calls)-2 {
		// Image data right after its width and height.
		*x = make([]byte, r.dims[0]*r.dims[1]*4)
		for i := range *x {
			(*x)[i] = byte(r.rng.Uint32())
		}
	}
	r.w.ByteSlice(x)
}

func (r *randomIO) Bytes(x *[]byte) {
	if !r.keep(r.begin()) {
		*x = r.randBytes()
	}
	if r.inUpdateTrade() {
		buf := new(bytes.Buffer)
		newNBTEncoder(buf, nbt.NetworkLittleEndian, nil).root(nbtGen{r.rng, rootKeys}.compound(0))
		*x = buf.Bytes()
	}
	r.w.Bytes(x)
}

// Composite methods built from primitives.

func (r *randomIO) Vec3(x *mgl32.Vec3) {
	r.Float32(&x[0])
	r.Float32(&x[1])
	r.Float32(&x[2])
}

func (r *randomIO) Vec2(x *mgl32.Vec2) {
	r.Float32(&x[0])
	r.Float32(&x[1])
}

func (r *randomIO) BlockPos(x *protocol.BlockPos) {
	r.Varint32(&x[0])
	r.Varint32(&x[1])
	r.Varint32(&x[2])
}

func (r *randomIO) ChunkPos(x *protocol.ChunkPos) {
	r.Varint32(&x[0])
	r.Varint32(&x[1])
}

func (r *randomIO) SubChunkPos(x *protocol.SubChunkPos) {
	r.Int32(&x[0])
	r.Int32(&x[1])
	r.Int32(&x[2])
}

// SoundPos: block coordinates b in +-2^20 so float32(b)/8 is exact; the Writer writes int32(x*8) = b.
func (r *randomIO) SoundPos(x *mgl32.Vec3) {
	if !r.keep(r.begin()) {
		for i := range x {
			x[i] = float32(r.rng.IntN(1<<21)-1<<20) / 8
		}
	}
	r.w.SoundPos(x)
}

// ByteFloat: any byte v; v*(360/256) is exact in float32 and the Writer maps it back to v.
func (r *randomIO) ByteFloat(x *float32) {
	if !r.keep(r.begin()) {
		*x = float32(uint8(r.rng.Uint32())) * (360.0 / 256.0)
	}
	r.w.ByteFloat(x)
}

// stableRGB holds the color channel values that survive Writer.RGB (v/255) and Reader.RGB (uint8(f*255)).
var stableRGB = func() []uint8 {
	var s []uint8
	for v := 0; v < 256; v++ {
		f := float32(v) / 255
		if uint8(f*255) == uint8(v) {
			s = append(s, uint8(v))
		}
	}
	return s
}()

func (r *randomIO) RGB(x *color.RGBA) {
	if !r.keep(r.begin()) {
		pick := func() uint8 { return stableRGB[r.rng.IntN(len(stableRGB))] }
		*x = color.RGBA{R: pick(), G: pick(), B: pick()}
	}
	start := r.out.Len()
	r.w.RGB(x)
	r.readBack(start, func(rd *protocol.Reader) { rd.RGB(x) })
}

func (r *randomIO) RGBA(x *color.RGBA) {
	if !r.keep(r.begin()) {
		v := r.rng.Uint32()
		*x = color.RGBA{R: byte(v), G: byte(v >> 8), B: byte(v >> 16), A: byte(v >> 24)}
	}
	r.w.RGBA(x)
}

func (r *randomIO) BEARGB(x *color.RGBA) {
	if !r.keep(r.begin()) {
		v := r.rng.Uint32()
		*x = color.RGBA{R: byte(v), G: byte(v >> 8), B: byte(v >> 16), A: byte(v >> 24)}
	}
	r.w.BEARGB(x)
}

func (r *randomIO) UUID(x *uuid.UUID) {
	if !r.keep(r.begin()) {
		for i := range x {
			x[i] = byte(r.rng.Uint32())
		}
	}
	r.w.UUID(x)
}

func (r *randomIO) NBT(m *map[string]any, encoding nbt.Encoding) {
	if !r.keep(r.begin()) {
		*m = nbtGen{r.rng, rootKeys}.compound(0)
	}
	start := r.out.Len()
	newNBTEncoder(r.out, encoding, nil).root(*m)
	r.readBack(start, func(rd *protocol.Reader) { rd.NBT(m, encoding) })
}

func (r *randomIO) NBTList(m *[]any, encoding nbt.Encoding) {
	if !r.keep(r.begin()) {
		*m = nbtGen{r.rng, rootKeys}.rootList()
	}
	start := r.out.Len()
	newNBTEncoder(r.out, encoding, nil).rootList(*m)
	r.readBack(start, func(rd *protocol.Reader) { rd.NBTList(m, encoding) })
}

// EntityMetadata: 0..4 distinct keys with random types and values; written sorted by key.
func (r *randomIO) EntityMetadata(x *protocol.EntityMetadata) {
	if !r.keep(r.begin()) {
		n := r.rng.IntN(5)
		m := protocol.EntityMetadata{}
		for len(m) < n {
			key := uint32(r.rng.IntN(140))
			if r.rng.IntN(4) == 0 {
				key = uint32(randBits(r.rng, 32))
			}
			m[key] = r.metadataValue()
		}
		*x = m
	}
	start := r.out.Len()
	r.w.EntityMetadata(x)
	r.readBack(start, func(rd *protocol.Reader) { rd.EntityMetadata(x) })
}

func (r *randomIO) metadataValue() any {
	switch r.rng.IntN(9) {
	case 0:
		return uint8(r.rng.Uint32())
	case 1:
		return int16(randBits(r.rng, 16))
	case 2:
		return int32(randBits(r.rng, 32))
	case 3:
		return randFloat32(r.rng)
	case 4:
		return randString(r.rng, 12)
	case 5:
		return nbtGen{r.rng, 1}.compound(0)
	case 6:
		return protocol.BlockPos{int32(randBits(r.rng, 32)), int32(randBits(r.rng, 32)), int32(randBits(r.rng, 32))}
	case 7:
		return int64(randBits(r.rng, 64))
	default:
		return mgl32.Vec3{randFloat32(r.rng), randFloat32(r.rng), randFloat32(r.rng)}
	}
}

// itemNetworkID returns 0 (air, no user data), the shield ID (blocking tick) or a random ID.
func (r *randomIO) itemNetworkID(bits uint) int32 {
	switch r.rng.IntN(5) {
	case 0:
		return 0
	case 1:
		return ShieldID
	default:
		v := randBits(r.rng, bits)
		if r.rng.IntN(2) == 0 {
			v = -v
		}
		if bits == 16 {
			return int32(int16(v))
		}
		return int32(v)
	}
}

// userData fills item user data: optional LittleEndian NBT, 0..2 StringUTF lists, blocking tick.
func (r *randomIO) userData(data *map[string]any, placeOn, canBreak *[]string, tick *int64) {
	*data = nil
	if r.rng.IntN(2) == 0 {
		*data = nbtGen{r.rng, 1}.compound(0)
	}
	list := func() []string {
		l := make([]string, r.rng.IntN(3))
		for i := range l {
			l[i] = randString(r.rng, 12)
		}
		return l
	}
	*placeOn, *canBreak = list(), list()
	*tick = int64(randBits(r.rng, 64))
}

func (r *randomIO) Item(x *protocol.ItemStack) {
	if !r.keep(r.begin()) {
		x.NetworkID = r.itemNetworkID(32)
		x.Count = uint16(r.rng.Uint32())
		x.MetadataValue = uint32(randBits(r.rng, 32))
		x.BlockRuntimeID = int32(randBits(r.rng, 32))
		r.userData(&x.NBTData, &x.CanBePlacedOn, &x.CanBreak, &x.BlockingTick)
	}
	start := r.out.Len()
	r.w.Item(x)
	r.readBack(start, func(rd *protocol.Reader) { rd.Item(x) })
}

func (r *randomIO) ItemInstance(i *protocol.ItemInstance) {
	if !r.keep(r.begin()) {
		x := &i.Stack
		x.NetworkID = r.itemNetworkID(16)
		x.Count = uint16(r.rng.Uint32())
		x.MetadataValue = uint32(randBits(r.rng, 32))
		x.BlockRuntimeID = int32(randBits(r.rng, 32))
		i.StackNetworkID = 0
		if r.rng.IntN(2) == 0 {
			i.StackNetworkID = int32(randBits(r.rng, 32))
		}
		r.userData(&x.NBTData, &x.CanBePlacedOn, &x.CanBreak, &x.BlockingTick)
	}
	start := r.out.Len()
	r.w.ItemInstance(i)
	r.readBack(start, func(rd *protocol.Reader) { rd.ItemInstance(i) })
}

func (r *randomIO) StackRequestItem(x *protocol.StackRequestItem) {
	if !r.keep(r.begin()) {
		switch r.rng.IntN(3) {
		case 0:
			x.Identifier = ""
		case 1:
			x.Identifier = "minecraft:shield"
		default:
			x.Identifier = "minecraft:" + randString(r.rng, 12)
		}
		x.MetadataValue = uint32(randBits(r.rng, 32))
		x.Count = uint16(r.rng.Uint32())
		x.BlockRuntimeID = int32(randBits(r.rng, 32))
		r.userData(&x.NBTData, &x.CanBePlacedOn, &x.CanBreak, &x.BlockingTick)
	}
	start := r.out.Len()
	r.w.StackRequestItem(x)
	r.readBack(start, func(rd *protocol.Reader) { rd.StackRequestItem(x) })
}

// ItemDescriptorCount: random descriptor kind; tags (variant, name, aux 32767) as Writer writes them;
// the descriptor body is marshalled with randomIO.
func (r *randomIO) ItemDescriptorCount(i *protocol.ItemDescriptorCount) {
	r.begin()
	var name string
	switch r.rng.IntN(4) {
	case 0:
		i.Descriptor = &protocol.InvalidItemDescriptor{}
	case 1:
		i.Descriptor, name = &protocol.DefaultItemDescriptor{}, "name"
	case 2:
		i.Descriptor, name = &protocol.MoLangItemDescriptor{}, "molang"
	default:
		i.Descriptor, name = &protocol.ItemTagItemDescriptor{}, "item_tag"
	}
	aux := int32(32767)
	if name == "" {
		variant := uint32(protocol.ItemDescriptorInvalid)
		r.w.Varuint32(&variant)
		r.w.Varint32(&aux)
	} else {
		variant := uint32(protocol.ItemDescriptorDefault)
		r.w.Varuint32(&variant)
		r.w.String(&name)
		i.Descriptor.Marshal(r)
		if name == "item_tag" {
			r.w.Varint32(&aux)
		}
	}
	r.Varint32(&i.Count)
}

// StackRequestAction: random readable action type; tag as Writer writes it; body marshalled randomly.
func (r *randomIO) StackRequestAction(x *protocol.StackRequestAction) {
	r.begin()
	e := tables.actions[r.rng.IntN(len(tables.actions))]
	*x = reflect.New(e.typ.Elem()).Interface().(protocol.StackRequestAction)
	r.w.Varuint32(&e.tag.variant)
	r.w.Uint8(&e.tag.id)
	(*x).Marshal(r)
}

// MaterialReducer: mix in 0..0x7fff (Reader decodes NetworkID as mix<<16, so only those round-trip).
func (r *randomIO) MaterialReducer(m *protocol.MaterialReducer) {
	r.begin()
	mix := int32(r.rng.IntN(0x8000))
	r.w.Varint32(&mix)
	m.InputItem = protocol.ItemType{NetworkID: mix << 16, MetadataValue: uint32(mix & 0x7fff)}
	protocol.Slice(r, &m.Outputs)
}

// EventType: the sample's event among those that also have an ordinal (the Event packet needs both).
func (r *randomIO) EventType(x *protocol.Event) {
	r.begin()
	e := tables.events[r.sample%len(tables.events)]
	*x = reflect.New(e.typ.Elem()).Interface().(protocol.Event)
	r.w.Varint32(&e.tag)
}

// EventOrdinal is fully determined by the event type.
func (r *randomIO) EventOrdinal(x *protocol.Event) {
	r.begin()
	r.w.EventOrdinal(x)
}

func (r *randomIO) TransactionDataType(x *protocol.InventoryTransactionData) {
	r.begin()
	e := tables.transactions[r.rng.IntN(len(tables.transactions))]
	*x = reflect.New(e.typ.Elem()).Interface().(protocol.InventoryTransactionData)
	r.w.Varuint32(&e.tag)
}

// PlayerInventoryAction: the legacy request, the actions, then the fields of the use item data's own
// Marshal.
func (r *randomIO) PlayerInventoryAction(x *protocol.UseItemTransactionData) {
	r.Varint32(&x.LegacyRequestID)
	protocol.OptionalFunc(r, &x.LegacySetItemSlots, func(slots *[]protocol.LegacySetItemSlot) {
		protocol.Slice(r, slots)
	})
	protocol.Slice(r, &x.Actions)
	x.Marshal(r)
}

// GameRule: random name, flag and value type (none, bool, uint32, float32).
func (r *randomIO) GameRule(x *protocol.GameRule) {
	if !r.keep(r.begin()) {
		x.Name = randString(r.rng, 12)
		x.CanBeModifiedByPlayer = r.rng.IntN(2) == 0
		switch r.rng.IntN(4) {
		case 0:
			x.Value = nil
		case 1:
			x.Value = r.rng.IntN(2) == 0
		case 2:
			x.Value = uint32(randBits(r.rng, 32))
		default:
			x.Value = randFloat32(r.rng)
		}
	}
	r.w.GameRule(x)
}

// AbilityValue: bool or float32; the unused half is written as the Writer's default.
func (r *randomIO) AbilityValue(x *any) {
	r.begin()
	if r.rng.IntN(2) == 0 {
		*x = r.rng.IntN(2) == 0
	} else {
		*x = randFloat32(r.rng)
	}
	r.w.AbilityValue(x)
}

// Bitset: random bits below size.
func (r *randomIO) Bitset(x *protocol.Bitset, size int) {
	b := protocol.NewBitset(size)
	if !r.keep(r.begin()) {
		for i := 0; i < size; i++ {
			if r.rng.IntN(4) == 0 {
				b.Set(i)
			}
		}
	}
	*x = b
	r.w.Bitset(x, size)
}

// PackSetting: random name and value (float32, bool, string, string list).
func (r *randomIO) PackSetting(x *protocol.PackSetting) {
	r.begin()
	x.Name = randString(r.rng, 12)
	switch r.rng.IntN(4) {
	case 0:
		x.Value = randFloat32(r.rng)
	case 1:
		x.Value = r.rng.IntN(2) == 0
	case 2:
		x.Value = randString(r.rng, 12)
	default:
		l := make([]string, r.rng.IntN(5))
		for i := range l {
			l[i] = randString(r.rng, 12)
		}
		x.Value = l
	}
	r.w.PackSetting(x)
}

// ShapeData: random shape type; body marshalled randomly.
func (r *randomIO) ShapeData(x *protocol.ShapeData) {
	r.begin()
	e := tables.shapes[r.rng.IntN(len(tables.shapes))]
	*x = reflect.New(e.typ.Elem()).Interface().(protocol.ShapeData)
	r.w.Varuint32(&e.tag)
	(*x).Marshal(r)
}

func (r *randomIO) ShieldID() int32 { return ShieldID }

func (r *randomIO) UnknownEnumOption(value any, enum string) {
	r.learnValue(value)
	panic(reject{"unknown enum option " + enum})
}

func (r *randomIO) InvalidValue(value any, forField, reason string) {
	r.learnValue(value)
	panic(reject{"invalid value for " + forField})
}

// SliceLength is called by protocol.SliceOfLen/FuncSliceOfLen before allocating; the length was drawn
// by the previous call, which is learned as a length site when the length is too big.
func (r *randomIO) SliceLength(value, max uint32) {
	if value > 4 || value > max {
		r.learnLast(modeLength)
		panic(reject{"slice length"})
	}
}
