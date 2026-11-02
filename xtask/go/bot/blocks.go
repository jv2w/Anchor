package main

import (
	"bytes"
	"encoding/binary"
	"errors"
	"fmt"
	"hash/fnv"
	"slices"
	"sort"

	"github.com/sandertv/gophertunnel/minecraft/protocol"
)

// blockState is a block name and its state properties, typed as in the block state list of the reference data.
type blockState struct {
	name  string
	props map[string]any
}

// knownStates are the states a flat world's spawn chunks hold. Their properties match the block state list
// of the reference data.
var knownStates = []blockState{
	{name: "minecraft:air"},
	{name: "minecraft:bedrock", props: map[string]any{"infiniburn_bit": uint8(0)}},
	{name: "minecraft:bedrock", props: map[string]any{"infiniburn_bit": uint8(1)}},
	{name: "minecraft:dirt"},
	{name: "minecraft:grass_block"},
	{name: "minecraft:stone"},
}

// networkBlockHash is the network block hash: FNV-1a 32 over the little-endian NBT of {name, states} with
// the state keys sorted. It returns an error instead of panicking on a property type NBT states do not use.
func networkBlockHash(name string, props map[string]any) (uint32, error) {
	if name == "minecraft:unknown" {
		return 0xfffffffe, nil
	}
	keys := make([]string, 0, len(props))
	for k := range props {
		keys = append(keys, k)
	}
	sort.Strings(keys)
	var b []byte
	str := func(s string) {
		b = binary.LittleEndian.AppendUint16(b, uint16(len(s)))
		b = append(b, s...)
	}
	b = append(b, 10, 0, 0) // compound with an empty name
	b = append(b, 8)
	str("name")
	str(name)
	b = append(b, 10)
	str("states")
	for _, k := range keys {
		switch v := props[k].(type) {
		case string:
			b = append(b, 8)
			str(k)
			str(v)
		case uint8:
			b = append(b, 1)
			str(k)
			b = append(b, v)
		case bool:
			b = append(b, 1)
			str(k)
			if v {
				b = append(b, 1)
			} else {
				b = append(b, 0)
			}
		case int32:
			b = append(b, 3)
			str(k)
			b = binary.LittleEndian.AppendUint32(b, uint32(v))
		default:
			return 0, fmt.Errorf("state %v of %v has unsupported type %T", k, name, v)
		}
	}
	b = append(b, 0, 0) // end of states, end of root
	h := fnv.New32a()
	_, _ = h.Write(b)
	return h.Sum32(), nil
}

// blockCheck collects the block palette entries seen in chunk data and matches them with knownStates.
type blockCheck struct {
	hashed          bool
	startGameBlocks int
	subChunks       int
	known           map[uint32]string
	seen            map[uint32]int
	errs            []string
}

func newBlockCheck() *blockCheck {
	c := &blockCheck{known: map[uint32]string{}, seen: map[uint32]int{}, errs: []string{}}
	for _, s := range knownStates {
		h, err := networkBlockHash(s.name, s.props)
		if err != nil {
			c.errs = append(c.errs, err.Error())
			continue
		}
		name := s.name
		if len(s.props) > 0 {
			name = fmt.Sprintf("%v%v", s.name, s.props)
		}
		c.known[h] = name
	}
	return c
}

func (c *blockCheck) fail(err error) {
	if len(c.errs) < 16 {
		c.errs = append(c.errs, err.Error())
	}
}

// levelChunk reads the palettes of the first count sub-chunks of a LevelChunk payload.
func (c *blockCheck) levelChunk(payload []byte, count int) {
	buf := bytes.NewBuffer(payload)
	for range count {
		if err := c.readSubChunk(buf); err != nil {
			c.fail(err)
			return
		}
	}
}

// subChunk reads the palettes of one SubChunk entry payload.
func (c *blockCheck) subChunk(payload []byte) {
	if err := c.readSubChunk(bytes.NewBuffer(payload)); err != nil {
		c.fail(err)
	}
}

// readSubChunk reads one network sub-chunk (versions 8 and 9) and counts its palette entries.
func (c *blockCheck) readSubChunk(buf *bytes.Buffer) error {
	version, err := buf.ReadByte()
	if err != nil {
		return err
	}
	if version != 8 && version != 9 {
		return fmt.Errorf("sub-chunk version %v", version)
	}
	layers, err := buf.ReadByte()
	if err != nil {
		return err
	}
	if version == 9 {
		if _, err := buf.ReadByte(); err != nil {
			return err
		}
	}
	for range layers {
		header, err := buf.ReadByte()
		if err != nil {
			return err
		}
		bits := int(header >> 1)
		if header&1 != 1 {
			return errors.New("palette is not in runtime (network) form")
		}
		if !slices.Contains(blockWidths, bits) {
			return fmt.Errorf("bits per block %v", bits)
		}
		words := 0
		if bits != 0 {
			words = 4096 / (32 / bits)
			if 4096%(32/bits) != 0 {
				words++
			}
		}
		if len(buf.Next(words*4)) != words*4 {
			return errors.New("short block storage")
		}
		var n int32 = 1
		if bits != 0 {
			if err := protocol.Varint32(buf, &n); err != nil {
				return err
			}
			if n <= 0 || n > 4096 {
				return fmt.Errorf("palette size %v", n)
			}
		}
		for range n {
			var v int32
			if err := protocol.Varint32(buf, &v); err != nil {
				return err
			}
			c.seen[uint32(v)]++
		}
	}
	c.subChunks++
	return nil
}

// json returns the "blocks" record: which palette entries match the hashes of known states.
func (c *blockCheck) json() map[string]any {
	matched := map[string]int{}
	unmatched := []string{}
	for v, n := range c.seen {
		if name, ok := c.known[v]; ok {
			matched[name] += n
		} else {
			unmatched = append(unmatched, fmt.Sprintf("%d", int32(v)))
		}
	}
	sort.Strings(unmatched)
	known := map[string]int64{}
	for h, name := range c.known {
		known[name] = int64(int32(h))
	}
	return map[string]any{
		"type":                        "blocks",
		"use_block_network_id_hashes": c.hashed,
		"start_game_blocks":           c.startGameBlocks,
		"sub_chunks":                  c.subChunks,
		"distinct_entries":            len(c.seen),
		"matched":                     matched,
		"unmatched":                   unmatched,
		"known_hashes":                known,
		"errors":                      c.errs,
	}
}
