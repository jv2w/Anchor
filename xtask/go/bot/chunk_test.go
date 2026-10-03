package main

import (
	"bytes"
	"encoding/binary"
	"strings"
	"testing"

	"github.com/sandertv/gophertunnel/minecraft/protocol"
)

// layer writes one runtime block storage of the given width whose blocks all have index (block % mod), then its
// palette.
func layer(bits int, mod int, palette []int32) []byte {
	var b bytes.Buffer
	b.WriteByte(byte(bits<<1 | 1))
	if bits != 0 {
		perWord := 32 / bits
		words := make([]uint32, (4096+perWord-1)/perWord)
		for i := range 4096 {
			words[i/perWord] |= uint32(i%mod) << (uint(i%perWord) * uint(bits))
		}
		_ = binary.Write(&b, binary.LittleEndian, words)
		n := int32(len(palette))
		protocol.WriteVarint32(&b, n)
	}
	for _, id := range palette {
		protocol.WriteVarint32(&b, id)
	}
	return b.Bytes()
}

func TestStrictSubChunkCheckAcceptsWhatAnchorReads(t *testing.T) {
	payload := append([]byte{9, 2, 0xfc}, layer(2, 3, []int32{-604749536, 5, 6})...)
	payload = append(payload, layer(0, 1, []int32{-604749536})...)
	payload = append(payload, 0x0a, 0) // block entity NBT after the layers
	rec := checkSubChunk(payload)
	if rec["strict"] != "ok" || rec["y_index"] != int8(-4) || rec["rest"] != 2 {
		t.Fatalf("got %v", rec)
	}
	if layers := rec["layers"].([]any); len(layers) != 2 || layers[0].(map[string]any)["bits"] != 2 {
		t.Fatalf("layers %v", layers)
	}
}

func TestContentHashIgnoresWidthAndPaletteOrder(t *testing.T) {
	content := func(payload []byte) any {
		rec := checkSubChunk(payload)
		return rec["layers"].([]any)[0].(map[string]any)["content_sha256"]
	}
	two := content(append([]byte{8, 1}, layer(1, 2, []int32{7, 9})...))
	wide := content(append([]byte{8, 1}, layer(4, 2, []int32{7, 9, 11})...))
	if two == nil || two != wide {
		t.Errorf("1 bit %v, 4 bits %v", two, wide)
	}
	if swapped := content(append([]byte{8, 1}, layer(1, 2, []int32{9, 7})...)); swapped == two {
		t.Error("different blocks have the same content hash")
	}
	if single := content([]byte{8, 1, 1, 14}); single == nil {
		t.Error("a 0-bit layer has no content hash")
	}
}

func TestStrictSubChunkCheckNamesTheBrokenRule(t *testing.T) {
	for _, c := range []struct {
		name    string
		payload []byte
		want    string
	}{
		{"version", []byte{7, 1, 1, 2}, "version 7"},
		{"persistent", []byte{8, 1, 0, 2}, "persistent palette"},
		{"width 7", []byte{8, 1, 7<<1 | 1}, "width 7"},
		{"palette too long", append([]byte{8, 1}, layer(1, 2, []int32{1, 2, 3})...), "palette length 3 for 1 bits"},
		{"index outside", append([]byte{8, 1}, layer(2, 4, []int32{1, 2, 3})...), "index 3 of 3"},
		{"short", []byte{8, 1, 1<<1 | 1, 0}, "short block storage"},
	} {
		got, _ := checkSubChunk(c.payload)["strict"].(string)
		if !strings.Contains(got, c.want) {
			t.Errorf("%v: got %q, want %q", c.name, got, c.want)
		}
	}
}

func TestOnlyTheChunksAroundTheSpawnAreRequested(t *testing.T) {
	s := &session{}
	if s.requestsSubChunks(protocol.ChunkPos{0, 0}) {
		t.Error("requested before StartGame")
	}
	s.spawnChunk, s.spawnKnown = chunkOf([3]float32{-0.5, 70, 31.9})
	if s.spawnChunk != (protocol.ChunkPos{-1, 1}) {
		t.Fatalf("spawn chunk %v", s.spawnChunk)
	}
	for pos, want := range map[protocol.ChunkPos]bool{{-1, 1}: true, {-2, 0}: true, {0, 2}: true, {1, 1}: false, {-1, 3}: false} {
		if s.requestsSubChunks(pos) != want {
			t.Errorf("%v: want %v", pos, want)
		}
	}
}
