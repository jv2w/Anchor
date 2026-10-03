package main

import (
	"bytes"
	"crypto/sha256"
	"encoding/binary"
	"encoding/hex"
	"errors"
	"fmt"
	"math"
	"slices"

	"github.com/sandertv/gophertunnel/minecraft/protocol"
	"github.com/sandertv/gophertunnel/minecraft/protocol/packet"
)

// Sub-chunk index range of the overworld (-64 to 319): what the bot requests in the probes.
const (
	lowestSubChunk  = -4
	highestSubChunk = 19
)

// unsentDistance is how many chunks away from the spawn chunk the "unsent" probe asks for, on x and z: farther
// than any view distance the bot asks for, so the server never sent that chunk.
const unsentDistance = 64

// blockWidths are the bits per block index Anchor's strict decoder reads (src/protocol/block_storage.rs).
var blockWidths = []int{0, 1, 2, 3, 4, 5, 6, 8, 16}

// checkSubChunk reads a network sub-chunk payload with the rules of Anchor's strict decoder
// (src/protocol/block_storage.rs): version 8 or 9, every layer in the runtime palette form (header bit 0 set),
// widths {0,1,2,3,4,5,6,8,16}, a palette of 1 to 2^bits entries and every block index inside the palette. It
// returns what it read: the version, the y index (version 9), each layer's width, palette and content hash
// (SHA-256 of its 4096 block network IDs as little-endian int32 in storage order, so that two encodings of the
// same blocks compare equal), the bytes after the layers (block entity NBT) and "strict": "ok" or the first
// rule the payload breaks.
func checkSubChunk(payload []byte) map[string]any {
	rec := map[string]any{}
	err := readStrictSubChunk(bytes.NewReader(payload), rec)
	if err != nil {
		rec["strict"] = err.Error()
	} else {
		rec["strict"] = "ok"
	}
	return rec
}

func readStrictSubChunk(r *bytes.Reader, rec map[string]any) error {
	version, err := r.ReadByte()
	if err != nil {
		return errors.New("no version byte")
	}
	rec["version"] = version
	if version != 8 && version != 9 {
		return fmt.Errorf("version %v", version)
	}
	count, err := r.ReadByte()
	if err != nil {
		return errors.New("no layer count")
	}
	if version == 9 {
		y, err := r.ReadByte()
		if err != nil {
			return errors.New("no y index")
		}
		rec["y_index"] = int8(y)
	}
	layers := make([]any, 0, count)
	defer func() { rec["layers"] = layers; rec["rest"] = r.Len() }()
	for i := range int(count) {
		layer, err := readStrictLayer(r)
		if layer != nil {
			layers = append(layers, layer)
		}
		if err != nil {
			return fmt.Errorf("layer %v: %w", i, err)
		}
	}
	return nil
}

// readStrictLayer reads one paletted block storage; it returns what it read so far on an error.
func readStrictLayer(r *bytes.Reader) (map[string]any, error) {
	header, err := r.ReadByte()
	if err != nil {
		return nil, errors.New("no storage header")
	}
	bits := int(header >> 1)
	layer := map[string]any{"bits": bits, "runtime": header&1 == 1}
	if header&1 == 0 {
		return layer, errors.New("persistent palette (header bit 0 clear)")
	}
	if !slices.Contains(blockWidths, bits) {
		return layer, fmt.Errorf("width %v", bits)
	}
	words := make([]uint32, 0)
	if bits != 0 {
		perWord := 32 / bits
		words = make([]uint32, (4096+perWord-1)/perWord)
		if err := binary.Read(r, binary.LittleEndian, words); err != nil {
			return layer, errors.New("short block storage")
		}
	}
	paletteLen := int64(1)
	if bits != 0 {
		var n int32
		if err := protocol.Varint32(r, &n); err != nil {
			return layer, errors.New("no palette length")
		}
		paletteLen = int64(n)
	}
	layer["palette_len"] = paletteLen
	if paletteLen < 1 || paletteLen > 1<<bits {
		return layer, fmt.Errorf("palette length %v for %v bits", paletteLen, bits)
	}
	palette := make([]int32, paletteLen)
	for i := range palette {
		if err := protocol.Varint32(r, &palette[i]); err != nil {
			return layer, errors.New("short palette")
		}
	}
	layer["palette"] = palette
	// The content: the 4096 block network IDs in storage order, which do not depend on the width or the palette
	// order.
	content := sha256.New()
	for i := range 4096 {
		index := uint32(0)
		if bits != 0 {
			perWord := 32 / bits
			index = words[i/perWord] >> (uint(i%perWord) * uint(bits)) & (uint32(1)<<bits - 1)
		}
		if int64(index) >= paletteLen {
			return layer, fmt.Errorf("block %v has index %v of %v palette entries", i, index, paletteLen)
		}
		_ = binary.Write(content, binary.LittleEndian, palette[index])
	}
	layer["content_sha256"] = hex.EncodeToString(content.Sum(nil))
	return layer, nil
}

// subChunkRecord is the "subchunk" line written after each SubChunk packet: per entry its offset, result and,
// when it has a payload, the strict check (checkSubChunk) with the payload length.
func subChunkRecord(index int, pk *packet.SubChunk) map[string]any {
	entries := make([]any, 0, len(pk.SubChunkEntries))
	for _, e := range pk.SubChunkEntries {
		entry := map[string]any{"offset": e.Offset[:], "result": e.Result}
		if payload, ok := e.RawPayload.Value(); ok {
			check := checkSubChunk(payload)
			check["len"] = len(payload)
			entry["payload"] = check
		}
		entries = append(entries, entry)
	}
	return map[string]any{"type": "subchunk", "index": index, "position": pk.Position[:], "entries": entries}
}

// chunkOf is the chunk that holds a position.
func chunkOf(pos [3]float32) (protocol.ChunkPos, bool) {
	x, z := math.Floor(float64(pos[0])/16), math.Floor(float64(pos[2])/16)
	if math.IsNaN(x) || math.IsNaN(z) || math.Abs(x) > 1<<24 || math.Abs(z) > 1<<24 {
		return protocol.ChunkPos{}, false
	}
	return protocol.ChunkPos{int32(x), int32(z)}, true
}

// requestsSubChunks reports whether the bot requests the sub-chunks of a chunk the server sent in the request
// mode: the 3x3 chunks around the spawn chunk, whatever order the server sends them in, so that both servers
// get the same requests.
func (s *session) requestsSubChunks(pos protocol.ChunkPos) bool {
	if !s.spawnKnown {
		return false
	}
	dx, dz := int64(pos.X())-int64(s.spawnChunk.X()), int64(pos.Z())-int64(s.spawnChunk.Z())
	return dx >= -1 && dx <= 1 && dz >= -1 && dz <= 1
}

// allOffsets are the offsets of every overworld sub-chunk index from index 0.
func allOffsets() []protocol.SubChunkOffset {
	var all []protocol.SubChunkOffset
	for y := lowestSubChunk; y <= highestSubChunk; y++ {
		all = append(all, protocol.SubChunkOffset{0, int8(y), 0})
	}
	return all
}

// probe sends the spawn scenario's extra sub-chunk requests once the bot is initialised: every sub-chunk index
// of the spawn chunk regardless of SubChunkLimit (the all-air ones too), one index below and one above the
// range, and a chunk the server never sent. The sent lines carry the probe name.
func (s *session) probe() error {
	if s.c.scenario != "spawn" || !s.spawnKnown {
		return nil
	}
	centre := protocol.SubChunkPos{s.spawnChunk.X(), 0, s.spawnChunk.Z()}
	probes := []struct {
		name string
		pk   *packet.SubChunkRequest
	}{
		{"all", &packet.SubChunkRequest{Position: centre, Offsets: allOffsets()}},
		{"out-of-range", &packet.SubChunkRequest{Position: centre, Offsets: []protocol.SubChunkOffset{
			{0, lowestSubChunk - 1, 0}, {0, highestSubChunk + 1, 0},
		}}},
		{"unsent", &packet.SubChunkRequest{
			Position: protocol.SubChunkPos{centre.X() + unsentDistance, 0, centre.Z() + unsentDistance},
			Offsets:  []protocol.SubChunkOffset{{0, lowestSubChunk, 0}, {0, 0, 0}},
		}},
	}
	for _, p := range probes {
		s.probeName = p.name
		err := s.send(p.pk)
		s.probeName = ""
		if err != nil {
			return err
		}
	}
	return nil
}
