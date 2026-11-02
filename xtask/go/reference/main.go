// Command reference converts the reference lists into the committed JSON files that
// `cargo xtask gen` and the tests read:
//
//	go run ./reference -reference-a DIR -reference-b DIR -out DIR
//
// DIR are the checkouts of reference list sources A and B at the pinned commits (`cargo xtask dump`
// checks the commits). It writes:
//   - blocks.json: every block state of source A's block_states.nbt with the network ID source B's
//     block_palette.nbt gives it. The two lists must hold exactly the same states (names, property names,
//     types and values); any difference fails the run.
//   - items.json: every item of source A's vanilla_items.nbt (name, runtime ID, component flag, version,
//     component data), sorted by runtime ID. Names, runtime IDs, versions and component flags must equal
//     source B's runtime_item_states.json.
//   - item_blocks.json: the block state the vanilla creative inventory (source B creative_items.json,
//     checked against source A's creative_items.nbt and the block list) gives each block item, for the
//     items with a single such state. The official server sends block items with these states.
//   - jigsaw_structure_data.dat, entity_properties.dat: source B's jigsaw structure data and entity
//     property definitions as the network NBT Anchor sends (network.go).
//
// NBT is read by a reader of its own so that list element types survive (gophertunnel's decoder drops the
// type of an empty list), and written in the same typed JSON as xtask/php/dump.php: {"byte": 1},
// {"list": {"type": "int", "values": [...]}}, {"compound": {...}}, floats as their bit patterns.
package main

import (
	"bytes"
	"compress/gzip"
	"encoding/binary"
	"encoding/json"
	"errors"
	"flag"
	"fmt"
	"io"
	"math"
	"os"
	"path/filepath"
	"slices"
	"sort"
	"strings"
)

func main() {
	sourceA := flag.String("reference-a", "", "reference list source A checkout")
	sourceB := flag.String("reference-b", "", "reference list source B checkout")
	out := flag.String("out", "", "output directory")
	flag.Parse()
	if *sourceA == "" || *sourceB == "" || *out == "" {
		fail(errors.New("usage: reference -reference-a DIR -reference-b DIR -out DIR"))
	}
	blocks, err := blockList(*sourceA, *sourceB)
	if err != nil {
		fail(err)
	}
	items, err := itemList(*sourceA, *sourceB)
	if err != nil {
		fail(err)
	}
	if err := writeLines(filepath.Join(*out, "blocks.json"), blocks); err != nil {
		fail(err)
	}
	if err := writeLines(filepath.Join(*out, "items.json"), items); err != nil {
		fail(err)
	}
	itemBlocks, err := itemBlockList(*sourceA, *sourceB, blocks)
	if err != nil {
		fail(err)
	}
	if err := writeLines(filepath.Join(*out, "item_blocks.json"), itemBlocks); err != nil {
		fail(err)
	}
	if err := writeNetworkCopies(*sourceB, *out); err != nil {
		fail(err)
	}
	fmt.Printf("reference: %d block states, %d items, %d item block states\n", len(blocks), len(items), len(itemBlocks))
}

func fail(err error) {
	fmt.Fprintln(os.Stderr, "reference:", err)
	os.Exit(1)
}

// block is one entry of blocks.json.
type block struct {
	Name      string         `json:"name"`
	States    map[string]any `json:"states"`
	Version   int32          `json:"version"`
	NetworkID uint32         `json:"network_id"`
}

// blockList reads both block lists, checks they hold the same states and returns them sorted by name and
// then by the JSON form of their states.
func blockList(sourceA, sourceB string) ([]block, error) {
	raw, err := os.ReadFile(filepath.Join(sourceA, "server", "world", "block_states.nbt"))
	if err != nil {
		return nil, err
	}
	r := &reader{b: raw, network: true}
	type key struct{ name, states string }
	fromA := map[key]block{}
	var order []key
	for r.len() > 0 {
		root, err := r.root()
		if err != nil {
			return nil, fmt.Errorf("block_states.nbt: %w", err)
		}
		b, err := stateOf(root)
		if err != nil {
			return nil, fmt.Errorf("block_states.nbt: %w", err)
		}
		k := key{b.Name, canonical(b.States)}
		if _, ok := fromA[k]; ok {
			return nil, fmt.Errorf("block_states.nbt: %v %v twice", k.name, k.states)
		}
		fromA[k] = b
		order = append(order, k)
	}

	gz, err := os.ReadFile(filepath.Join(sourceB, "block_palette.nbt"))
	if err != nil {
		return nil, err
	}
	zr, err := gzip.NewReader(bytes.NewReader(gz))
	if err != nil {
		return nil, fmt.Errorf("block_palette.nbt: %w", err)
	}
	plain, err := io.ReadAll(zr)
	if err != nil {
		return nil, fmt.Errorf("block_palette.nbt: %w", err)
	}
	r = &reader{b: plain, bigEndian: true}
	root, err := r.root()
	if err != nil {
		return nil, fmt.Errorf("block_palette.nbt: %w", err)
	}
	if r.len() != 0 {
		return nil, errors.New("block_palette.nbt: bytes after the root")
	}
	list, ok := field[map[string]any](root, "blocks", "list")
	if !ok {
		return nil, errors.New("block_palette.nbt: no blocks list")
	}
	values, _ := list["values"].([]any)
	seen := map[key]bool{}
	for i, v := range values {
		c, ok := v.(map[string]any)["compound"].(map[string]any)
		if !ok {
			return nil, fmt.Errorf("block_palette.nbt: entry %d is not a compound", i)
		}
		b, err := stateOf(c)
		if err != nil {
			return nil, fmt.Errorf("block_palette.nbt: entry %d: %w", i, err)
		}
		id, ok := field[int32](c, "network_id", "int")
		if !ok {
			return nil, fmt.Errorf("block_palette.nbt: entry %d has no network_id", i)
		}
		k := key{b.Name, canonical(b.States)}
		d, ok := fromA[k]
		if !ok {
			return nil, fmt.Errorf("source B has %v %v, source A does not", k.name, k.states)
		}
		if seen[k] {
			return nil, fmt.Errorf("block_palette.nbt: %v %v twice", k.name, k.states)
		}
		if d.Version != b.Version {
			return nil, fmt.Errorf("%v %v: version %d in source A, %d in source B", k.name, k.states, d.Version, b.Version)
		}
		seen[k] = true
		d.NetworkID = uint32(id)
		fromA[k] = d
	}
	var out []block
	for _, k := range order {
		if !seen[k] {
			return nil, fmt.Errorf("source A has %v %v, source B does not", k.name, k.states)
		}
	}
	slices.SortFunc(order, func(a, b key) int {
		if c := strings.Compare(a.name, b.name); c != 0 {
			return c
		}
		return strings.Compare(a.states, b.states)
	})
	for _, k := range order {
		out = append(out, fromA[k])
	}
	return out, nil
}

// stateOf reads name, states and version from a block state compound.
func stateOf(c map[string]any) (block, error) {
	name, ok := field[string](c, "name", "string")
	if !ok {
		return block{}, errors.New("state without a name")
	}
	states, ok := field[map[string]any](c, "states", "compound")
	if !ok {
		return block{}, fmt.Errorf("%v has no states compound", name)
	}
	for k, v := range states {
		t := tagName(v)
		if t != "byte" && t != "int" && t != "string" {
			return block{}, fmt.Errorf("%v state %v has type %v", name, k, t)
		}
	}
	version, ok := field[int32](c, "version", "int")
	if !ok {
		return block{}, fmt.Errorf("%v has no version", name)
	}
	return block{Name: name, States: states, Version: version}, nil
}

// item is one entry of items.json.
type item struct {
	Name           string         `json:"name"`
	RuntimeID      int32          `json:"runtime_id"`
	ComponentBased bool           `json:"component_based"`
	Version        int32          `json:"version"`
	Data           map[string]any `json:"data"`
}

// itemList reads source A's item list, checks it against source B's and returns it sorted by runtime
// ID.
func itemList(sourceA, sourceB string) ([]item, error) {
	raw, err := os.ReadFile(filepath.Join(sourceA, "server", "world", "vanilla_items.nbt"))
	if err != nil {
		return nil, err
	}
	r := &reader{b: raw, network: true}
	root, err := r.root()
	if err != nil {
		return nil, fmt.Errorf("vanilla_items.nbt: %w", err)
	}
	if r.len() != 0 {
		return nil, errors.New("vanilla_items.nbt: bytes after the root")
	}
	var items []item
	for name, v := range root {
		c, ok := v.(map[string]any)["compound"].(map[string]any)
		if !ok {
			return nil, fmt.Errorf("vanilla_items.nbt: %v is not a compound", name)
		}
		id, ok1 := field[int32](c, "runtime_id", "int")
		component, ok2 := field[int8](c, "component_based", "byte")
		version, ok3 := field[int32](c, "version", "int")
		if !ok1 || !ok2 || !ok3 || component < 0 || component > 1 {
			return nil, fmt.Errorf("vanilla_items.nbt: %v lacks runtime_id, component_based or version", name)
		}
		data := map[string]any{}
		if d, ok := field[map[string]any](c, "data", "compound"); ok {
			data = d
		}
		for k := range c {
			if k != "runtime_id" && k != "component_based" && k != "version" && k != "data" {
				return nil, fmt.Errorf("vanilla_items.nbt: %v has unknown key %v", name, k)
			}
		}
		items = append(items, item{Name: name, RuntimeID: id, ComponentBased: component == 1, Version: version, Data: data})
	}
	sort.Slice(items, func(i, j int) bool { return items[i].RuntimeID < items[j].RuntimeID })
	for i := 1; i < len(items); i++ {
		if items[i].RuntimeID == items[i-1].RuntimeID {
			return nil, fmt.Errorf("vanilla_items.nbt: runtime ID %d twice", items[i].RuntimeID)
		}
	}

	js, err := os.ReadFile(filepath.Join(sourceB, "runtime_item_states.json"))
	if err != nil {
		return nil, err
	}
	var cb []struct {
		Name           string `json:"name"`
		ID             int32  `json:"id"`
		Version        int32  `json:"version"`
		ComponentBased bool   `json:"componentBased"`
	}
	dec := json.NewDecoder(bytes.NewReader(js))
	dec.DisallowUnknownFields()
	if err := dec.Decode(&cb); err != nil {
		return nil, fmt.Errorf("runtime_item_states.json: %w", err)
	}
	if len(cb) != len(items) {
		return nil, fmt.Errorf("source A has %d items, source B %d", len(items), len(cb))
	}
	byName := map[string]item{}
	for _, it := range items {
		byName[it.Name] = it
	}
	for _, c := range cb {
		it, ok := byName[c.Name]
		if !ok {
			return nil, fmt.Errorf("source B has item %v, source A does not", c.Name)
		}
		if it.RuntimeID != c.ID || it.Version != c.Version || it.ComponentBased != c.ComponentBased {
			return nil, fmt.Errorf("item %v differs: source A %+v, source B %+v", c.Name, it, c)
		}
	}
	return items, nil
}

// field returns the value of a typed NBT field of a compound.
func field[T any](c map[string]any, name, typ string) (T, bool) {
	var zero T
	v, ok := c[name].(map[string]any)
	if !ok {
		return zero, false
	}
	x, ok := v[typ].(T)
	return x, ok
}

func tagName(v any) string {
	for k := range v.(map[string]any) {
		return k
	}
	return ""
}

// canonical is the JSON form of a typed value; encoding/json sorts map keys, so equal values give equal
// text.
func canonical(v any) string {
	b, err := json.Marshal(v)
	if err != nil {
		fail(err)
	}
	return string(b)
}

// writeLines writes a list with one compact JSON entry per line, as xtask/php/dump.php does.
func writeLines[T any](path string, list []T) error {
	var b strings.Builder
	b.WriteString("[\n")
	for i, v := range list {
		var line bytes.Buffer
		enc := json.NewEncoder(&line)
		enc.SetEscapeHTML(false)
		if err := enc.Encode(v); err != nil {
			return err
		}
		b.WriteString(strings.TrimSuffix(line.String(), "\n"))
		if i+1 < len(list) {
			b.WriteString(",")
		}
		b.WriteString("\n")
	}
	b.WriteString("]\n")
	return os.WriteFile(path, []byte(b.String()), 0o644)
}

// NBT tag types.
const (
	tagEnd = iota
	tagByte
	tagShort
	tagInt
	tagLong
	tagFloat
	tagDouble
	tagByteArray
	tagString
	tagList
	tagCompound
	tagIntArray
	tagLongArray
)

var typeNames = [...]string{"end", "byte", "short", "int", "long", "float", "double", "byte_array", "string", "list", "compound", "int_array", "long_array"}

// reader reads NBT in the network little-endian (varint lengths and ints), little-endian or big-endian
// encoding into typed values. Input is trusted (pinned files) but bounds are still checked.
type reader struct {
	b         []byte
	network   bool
	bigEndian bool
}

var errShort = errors.New("unexpected end of NBT")

func (r *reader) len() int { return len(r.b) }

func (r *reader) take(n int) ([]byte, error) {
	if n < 0 || n > len(r.b) {
		return nil, errShort
	}
	v := r.b[:n]
	r.b = r.b[n:]
	return v, nil
}

func (r *reader) u8() (uint8, error) {
	b, err := r.take(1)
	if err != nil {
		return 0, err
	}
	return b[0], nil
}

func (r *reader) order() binary.ByteOrder {
	if r.bigEndian {
		return binary.BigEndian
	}
	return binary.LittleEndian
}

func (r *reader) varuint() (uint64, error) {
	var v uint64
	for shift := 0; shift < 70; shift += 7 {
		b, err := r.u8()
		if err != nil {
			return 0, err
		}
		v |= uint64(b&0x7f) << shift
		if b&0x80 == 0 {
			return v, nil
		}
	}
	return 0, errors.New("varint too long")
}

func (r *reader) i32() (int32, error) {
	if r.network {
		u, err := r.varuint()
		return int32(uint32(u>>1) ^ -uint32(u&1)), err
	}
	b, err := r.take(4)
	if err != nil {
		return 0, err
	}
	return int32(r.order().Uint32(b)), nil
}

func (r *reader) i64() (int64, error) {
	if r.network {
		u, err := r.varuint()
		return int64(u>>1) ^ -int64(u&1), err
	}
	b, err := r.take(8)
	if err != nil {
		return 0, err
	}
	return int64(r.order().Uint64(b)), nil
}

// length reads a list or array length: a signed varint32 in the network encoding.
func (r *reader) length() (int, error) {
	n, err := r.i32()
	if n < 0 {
		return 0, errors.New("negative length")
	}
	return int(n), err
}

func (r *reader) str() (string, error) {
	var n int
	if r.network {
		u, err := r.varuint()
		if err != nil {
			return "", err
		}
		if u > math.MaxInt16 {
			return "", errors.New("string too long")
		}
		n = int(u)
	} else {
		b, err := r.take(2)
		if err != nil {
			return "", err
		}
		n = int(r.order().Uint16(b))
	}
	b, err := r.take(n)
	return string(b), err
}

// root reads a named root compound and returns its contents.
func (r *reader) root() (map[string]any, error) {
	t, err := r.u8()
	if err != nil {
		return nil, err
	}
	if t != tagCompound {
		return nil, fmt.Errorf("root tag %d is not a compound", t)
	}
	if _, err := r.str(); err != nil {
		return nil, err
	}
	return r.compound(0)
}

func (r *reader) compound(depth int) (map[string]any, error) {
	if depth > 512 {
		return nil, errors.New("NBT too deep")
	}
	c := map[string]any{}
	for {
		t, err := r.u8()
		if err != nil {
			return nil, err
		}
		if t == tagEnd {
			return c, nil
		}
		name, err := r.str()
		if err != nil {
			return nil, err
		}
		v, err := r.value(t, depth+1)
		if err != nil {
			return nil, fmt.Errorf("%v: %w", name, err)
		}
		c[name] = v
	}
}

// value reads one payload of type t as {"type": value}.
func (r *reader) value(t uint8, depth int) (map[string]any, error) {
	if int(t) >= len(typeNames) || t == tagEnd {
		return nil, fmt.Errorf("bad tag type %d", t)
	}
	var v any
	var err error
	switch t {
	case tagByte:
		var b uint8
		b, err = r.u8()
		v = int8(b)
	case tagShort:
		var b []byte
		b, err = r.take(2)
		if err == nil {
			v = int16(r.order().Uint16(b))
		}
	case tagInt:
		v, err = r.i32()
	case tagLong:
		v, err = r.i64()
	case tagFloat:
		var b []byte
		b, err = r.take(4)
		if err == nil {
			v = r.order().Uint32(b)
		}
	case tagDouble:
		var b []byte
		b, err = r.take(8)
		if err == nil {
			v = r.order().Uint64(b)
		}
	case tagByteArray:
		var n int
		n, err = r.length()
		if err == nil {
			var b []byte
			b, err = r.take(n)
			arr := make([]int, len(b))
			for i, x := range b {
				arr[i] = int(x)
			}
			v = arr
		}
	case tagString:
		v, err = r.str()
	case tagList:
		var et uint8
		et, err = r.u8()
		if err != nil {
			return nil, err
		}
		var n int
		n, err = r.length()
		if err != nil {
			return nil, err
		}
		if int(et) >= len(typeNames) || (et == tagEnd && n > 0) {
			return nil, fmt.Errorf("bad list element type %d", et)
		}
		values := []any{}
		for i := 0; i < n; i++ {
			x, err := r.value(et, depth+1)
			if err != nil {
				return nil, err
			}
			values = append(values, x)
		}
		v = map[string]any{"type": typeNames[et], "values": values}
	case tagCompound:
		v, err = r.compound(depth)
	case tagIntArray, tagLongArray:
		var n int
		n, err = r.length()
		if err != nil {
			return nil, err
		}
		arr := []any{}
		for i := 0; i < n && err == nil; i++ {
			var x any
			if t == tagIntArray {
				x, err = r.i32()
			} else {
				x, err = r.i64()
			}
			arr = append(arr, x)
		}
		v = arr
	}
	if err != nil {
		return nil, err
	}
	return map[string]any{typeNames[t]: v}, nil
}
