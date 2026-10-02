package main

import (
	"bytes"
	"encoding/binary"
	"fmt"
	"math/rand/v2"
	"os"
	"reflect"
	"sort"

	"github.com/sandertv/gophertunnel/minecraft/protocol"
	"github.com/sandertv/gophertunnel/minecraft/protocol/packet"
)

// maxAttempts caps the attempts for one sample.
const maxAttempts = 10000

type actionTag struct {
	variant uint32
	id      uint8
}

type unionEntry[T any] struct {
	tag T
	typ reflect.Type
}

// tables holds the union variants gophertunnel's Reader accepts, found by probing the Reader with every
// tag (the lookup tables themselves are unexported).
var tables = probeTables()

type unionTables struct {
	actions      []unionEntry[actionTag]
	actionTag    map[reflect.Type]actionTag
	events       []unionEntry[int32]
	transactions []unionEntry[uint32]
	shapes       []unionEntry[uint32]
	shapeTag     map[reflect.Type]uint32
	warnings     []string
}

// try runs f and returns the panic it raised as an error, if any.
func try(f func()) (err error) {
	defer func() {
		if v := recover(); v != nil {
			switch v := v.(type) {
			case error:
				err = v
			case reject:
				err = fmt.Errorf("%s", v.reason)
			default:
				err = fmt.Errorf("%v", v)
			}
		}
	}()
	f()
	return nil
}

func reader(b []byte) *protocol.Reader {
	return protocol.NewReader(bytes.NewReader(b), ShieldID, true)
}

func probeTables() unionTables {
	t := unionTables{actionTag: map[reflect.Type]actionTag{}, shapeTag: map[reflect.Type]uint32{}}
	for v := uint32(0); v < 256; v++ {
		var x protocol.StackRequestAction
		_ = try(func() { reader(append(binary.AppendUvarint(nil, uint64(v)), 0)).StackRequestAction(&x) })
		if x == nil {
			continue
		}
		// Take the tag the Writer writes for this type; keep it only if it reads back as the same variant.
		buf := new(bytes.Buffer)
		fresh := reflect.New(reflect.TypeOf(x).Elem()).Interface().(protocol.StackRequestAction)
		if try(func() { protocol.NewWriter(buf, ShieldID).StackRequestAction(&fresh) }) != nil {
			continue
		}
		variant, n := binary.Uvarint(buf.Bytes())
		if uint32(variant) != v || n <= 0 || n >= buf.Len() {
			t.warnings = append(t.warnings, fmt.Sprintf("stack request action %T: written variant %d != read variant %d", x, variant, v))
			continue
		}
		tag := actionTag{variant: v, id: buf.Bytes()[n]}
		t.actions = append(t.actions, unionEntry[actionTag]{tag, reflect.TypeOf(x)})
		t.actionTag[reflect.TypeOf(x)] = tag
	}
	for v := int32(-64); v < 1024; v++ {
		var x protocol.Event
		var b []byte
		b = binary.AppendUvarint(b, uint64(uint32(v<<1)^uint32(v>>31)))
		_ = try(func() { reader(b).EventType(&x) })
		if x == nil {
			continue
		}
		// The Event packet also writes an ordinal; events without one cannot be read back.
		if try(func() { protocol.NewWriter(new(bytes.Buffer), ShieldID).EventOrdinal(&x) }) != nil {
			t.warnings = append(t.warnings, fmt.Sprintf("event %T (type %d) has no ordinal; skipped", x, v))
			continue
		}
		t.events = append(t.events, unionEntry[int32]{v, reflect.TypeOf(x)})
	}
	for v := uint32(0); v < 256; v++ {
		var x protocol.InventoryTransactionData
		_ = try(func() { reader(binary.AppendUvarint(nil, uint64(v))).TransactionDataType(&x) })
		if x != nil {
			t.transactions = append(t.transactions, unionEntry[uint32]{v, reflect.TypeOf(x)})
		}
	}
	for v := uint32(0); v < 256; v++ {
		var x protocol.ShapeData
		_ = try(func() { reader(binary.AppendUvarint(nil, uint64(v))).ShapeData(&x) })
		if x != nil {
			t.shapes = append(t.shapes, unionEntry[uint32]{v, reflect.TypeOf(x)})
			t.shapeTag[reflect.TypeOf(x)] = v
		}
	}
	return t
}

// packetPool returns the union of gophertunnel's client and server pools.
func packetPool() map[uint32]func() packet.Packet {
	pool := map[uint32]func() packet.Packet{}
	for id, f := range packet.NewServerPool() {
		pool[id] = f
	}
	for id, f := range packet.NewClientPool() {
		pool[id] = f
	}
	return pool
}

func sortedIDs(pool map[uint32]func() packet.Packet) []uint32 {
	ids := make([]uint32, 0, len(pool))
	for id := range pool {
		ids = append(ids, id)
	}
	sort.Slice(ids, func(i, j int) bool { return ids[i] < ids[j] })
	return ids
}

// decode decodes b with gophertunnel (limits enabled) and requires every byte to be consumed. On failure
// it also returns the offset the Reader stopped at.
func decode(newPk func() packet.Packet, b []byte) (packet.Packet, int, error) {
	pk := newPk()
	br := bytes.NewReader(b)
	if err := try(func() { pk.Marshal(protocol.NewReader(br, ShieldID, true)) }); err != nil {
		return nil, len(b) - br.Len(), err
	}
	if br.Len() != 0 {
		return nil, len(b) - br.Len(), fmt.Errorf("%d unread bytes", br.Len())
	}
	return pk, 0, nil
}

// canonEncode encodes pk with canonWriter and returns the bytes and the number of multi-key compounds.
func canonEncode(pk packet.Packet) ([]byte, int, error) {
	buf := new(bytes.Buffer)
	cw := newCanonWriter(buf)
	err := try(func() { pk.Marshal(cw) })
	return buf.Bytes(), cw.multiKey, err
}

// keepProb is the chance that any call keeps the default value at the given attempt.
func keepProb(attempt int) float64 {
	if attempt < 50 {
		return 0
	}
	return min(0.95, float64(attempt-50)/2000)
}

// genSample produces one sample that gophertunnel decodes fully and that canonWriter re-encodes to the
// same bytes. ok is false if every attempt failed.
func genSample(r *randomIO, newPk func() packet.Packet) ([]byte, int, bool) {
	for attempt := 0; attempt < maxAttempts; attempt++ {
		r.reset(keepProb(attempt))
		if try(func() { newPk().Marshal(r) }) != nil {
			continue
		}
		sample := bytes.Clone(r.out.Bytes())
		pk, off, err := decode(newPk, sample)
		if err != nil {
			r.learnAt(max(off-1, 0))
			continue
		}
		again, _, err := canonEncode(pk)
		if err != nil {
			r.learnAt(len(sample))
			continue
		}
		if !bytes.Equal(again, sample) {
			r.learnAt(firstDiff(again, sample))
			continue
		}
		return sample, attempt + 1, true
	}
	return nil, maxAttempts, false
}

func firstDiff(a, b []byte) int {
	n := min(len(a), len(b))
	for i := 0; i < n; i++ {
		if a[i] != b[i] {
			return i
		}
	}
	return n
}

// corpus generates samples for every packet ID in ascending order. Each packet has its own PCG seeded
// from (seed, packet ID), so packets are independent of each other.
func corpus(seed uint64, samples int, verbose bool) []record {
	for _, w := range tables.warnings {
		fmt.Fprintln(os.Stderr, "warning:", w)
	}
	pool := packetPool()
	var out []record
	for _, id := range sortedIDs(pool) {
		r := newRandomIO(rand.New(rand.NewPCG(seed, uint64(id))))
		total := 0
		got := 0
		for s := 0; s < samples; s++ {
			b, attempts, ok := genSample(r, pool[id])
			total += attempts
			if !ok {
				break
			}
			out = append(out, record{id: id, data: b})
			got++
		}
		if got < samples {
			fmt.Fprintf(os.Stderr, "warning: packet %d (%T): %d/%d samples\n", id, pool[id](), got, samples)
		}
		if verbose {
			fmt.Fprintf(os.Stderr, "packet %d (%T): %d samples, %d attempts\n", id, pool[id](), got, total)
		}
	}
	return out
}

// check decodes each record with gophertunnel and re-encodes it with gophertunnel's Writer.
func check(in []record) []record {
	pool := packetPool()
	out := make([]record, 0, len(in))
	for _, rec := range in {
		newPk, ok := pool[rec.id]
		if !ok {
			out = append(out, failure(fmt.Errorf("unknown packet id %d", rec.id)))
			continue
		}
		pk, _, err := decode(newPk, rec.data)
		if err != nil {
			out = append(out, failure(err))
			continue
		}
		buf := new(bytes.Buffer)
		if err := try(func() { pk.Marshal(protocol.NewWriter(buf, ShieldID)) }); err != nil {
			out = append(out, failure(fmt.Errorf("encode: %w", err)))
			continue
		}
		out = append(out, record{id: 0, data: buf.Bytes()})
	}
	return out
}
