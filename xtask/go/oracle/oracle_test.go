package main

import (
	"bytes"
	"fmt"
	"math/rand/v2"
	"testing"
)

func encodeRecords(recs []record) []byte {
	var b bytes.Buffer
	for _, r := range recs {
		b.Write([]byte{byte(r.id), byte(r.id >> 8), byte(r.id >> 16), byte(r.id >> 24)})
		n := len(r.data)
		b.Write([]byte{byte(n), byte(n >> 8), byte(n >> 16), byte(n >> 24)})
		b.Write(r.data)
	}
	return b.Bytes()
}

func TestCorpusIsDeterministicAndFullyCovered(t *testing.T) {
	const samples = 64
	a := corpus(1, samples, false)
	b := corpus(1, samples, false)
	if !bytes.Equal(encodeRecords(a), encodeRecords(b)) {
		t.Fatal("corpus output differs between runs")
	}
	counts := map[uint32]int{}
	distinct := map[uint32]map[string]bool{}
	for _, r := range a {
		counts[r.id]++
		if distinct[r.id] == nil {
			distinct[r.id] = map[string]bool{}
		}
		distinct[r.id][string(r.data)] = true
	}
	pool := packetPool()
	for _, id := range sortedIDs(pool) {
		if counts[id] != samples {
			t.Errorf("packet %d (%T): %d samples", id, pool[id](), counts[id])
		}
		if testing.Verbose() && len(distinct[id]) < samples/2 {
			t.Logf("packet %d (%T): %d distinct samples", id, pool[id](), len(distinct[id]))
		}
	}
	t.Logf("%d records, %d packets", len(a), len(counts))
}

// TestCheckRoundTrip: every corpus record decodes in check; re-encoded bytes differ from the input only
// for samples holding multi-key NBT compounds (gophertunnel writes map keys in random order).
func TestCheckRoundTrip(t *testing.T) {
	in := corpus(1, 64, false)
	out := check(in)
	pool := packetPool()
	mismatch, multiKeyMismatch := 0, 0
	for i, r := range out {
		if r.id != 0 {
			t.Fatalf("record %d (packet %d): %s", i, in[i].id, r.data)
		}
		if bytes.Equal(r.data, in[i].data) {
			continue
		}
		mismatch++
		pk, _, err := decode(pool[in[i].id], in[i].data)
		if err != nil {
			t.Fatal(err)
		}
		_, multiKey, _ := canonEncode(pk)
		if multiKey > 0 && len(r.data) == len(in[i].data) {
			multiKeyMismatch++
			continue
		}
		t.Errorf("record %d (packet %d %T): re-encode differs without multi-key NBT", i, in[i].id, pk)
	}
	t.Logf("%d records, %d re-encodes differ, all %d from multi-key NBT order", len(in), mismatch, multiKeyMismatch)
}

func TestCompressionRoundTrip(t *testing.T) {
	rng := rand.New(rand.NewPCG(7, 7))
	var in []record
	for _, n := range []int{0, 1, 255, 256, 65536, 1 << 20} {
		random := make([]byte, n)
		for i := range random {
			random[i] = byte(rng.Uint32())
		}
		repeated := bytes.Repeat([]byte("anchor"), n/6+1)[:n]
		in = append(in, record{data: random}, record{data: repeated})
	}
	for _, algo := range []string{"zlib", "snappy"} {
		c, err := compression(algo)
		if err != nil {
			t.Fatal(err)
		}
		packed := transform(in, c.Compress)
		unpacked := transform(packed, func(b []byte) ([]byte, error) { return c.Decompress(b, decompressLimit) })
		for i := range in {
			if packed[i].id != 0 || unpacked[i].id != 0 || !bytes.Equal(unpacked[i].data, in[i].data) {
				t.Errorf("%s: record %d (%d bytes) did not round-trip", algo, i, len(in[i].data))
			}
		}
		bad := transform([]record{{data: []byte{0xff, 0xff, 0xff}}}, func(b []byte) ([]byte, error) { return c.Decompress(b, decompressLimit) })
		if bad[0].id != 1 {
			t.Errorf("%s: garbage input decompressed", algo)
		}
		fmt.Printf("%s: %d sizes ok, garbage error: %s\n", algo, len(in), bad[0].data)
	}
}
