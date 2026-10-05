package main

import (
	"bytes"
	"compress/gzip"
	"encoding/binary"
	"errors"
	"fmt"
	"io"
	"os"
	"path/filepath"
	"sort"
)

// The CloudburstMC files Anchor sends as packets, and the network NBT files written from them.
const (
	jigsawSource     = "jigsaw_structure_data.nbt"
	jigsawOut        = "jigsaw_structure_data.dat"
	propertiesSource = "entity_properties.nbt"
	propertiesOut    = "entity_properties.dat"
)

// writeNetworkCopies writes the CloudburstMC data BDS sends in the spawn window (PLAN.md 10.6) as the network
// little-endian NBT of the packets, compound keys in byte order as Anchor writes them:
//   - jigsaw_structure_data.dat: jigsaw_structure_data.nbt, the StructureData of JigsawStructureData.
//   - entity_properties.dat: each compound of entity_properties.nbt as one root, in the file's order (BDS's
//     order), the PropertyData of one SyncActorProperty each.
func writeNetworkCopies(cloudburst, out string) error {
	r, err := gzipBigEndian(filepath.Join(cloudburst, jigsawSource))
	if err != nil {
		return err
	}
	jigsaw, err := r.root()
	if err != nil {
		return fmt.Errorf("%v: %w", jigsawSource, err)
	}
	if r.len() != 0 {
		return fmt.Errorf("%v: bytes after the root", jigsawSource)
	}
	var w networkWriter
	if err := w.root(jigsaw); err != nil {
		return fmt.Errorf("%v: %w", jigsawSource, err)
	}
	if err := os.WriteFile(filepath.Join(out, jigsawOut), w.b.Bytes(), 0o644); err != nil {
		return err
	}

	r, err = gzipBigEndian(filepath.Join(cloudburst, propertiesSource))
	if err != nil {
		return err
	}
	entries, err := r.rootEntries()
	if err != nil {
		return fmt.Errorf("%v: %w", propertiesSource, err)
	}
	if r.len() != 0 {
		return fmt.Errorf("%v: bytes after the root", propertiesSource)
	}
	w = networkWriter{}
	for _, e := range entries {
		c, ok := e.value["compound"].(map[string]any)
		if !ok {
			return fmt.Errorf("%v: %v is not a compound", propertiesSource, e.name)
		}
		if typ, _ := field[string](c, "type", "string"); typ != e.name {
			return fmt.Errorf("%v: %v has type %q", propertiesSource, e.name, typ)
		}
		if err := w.root(c); err != nil {
			return fmt.Errorf("%v: %v: %w", propertiesSource, e.name, err)
		}
	}
	return os.WriteFile(filepath.Join(out, propertiesOut), w.b.Bytes(), 0o644)
}

// gzipBigEndian returns a big-endian reader over a gzip file's contents.
func gzipBigEndian(path string) (*reader, error) {
	gz, err := os.ReadFile(path)
	if err != nil {
		return nil, err
	}
	zr, err := gzip.NewReader(bytes.NewReader(gz))
	if err != nil {
		return nil, fmt.Errorf("%v: %w", path, err)
	}
	plain, err := io.ReadAll(zr)
	if err != nil {
		return nil, fmt.Errorf("%v: %w", path, err)
	}
	return &reader{b: plain, bigEndian: true}, nil
}

// entry is one named value of a compound, in file order.
type entry struct {
	name  string
	value map[string]any
}

// rootEntries reads a named root compound and returns its entries in file order.
func (r *reader) rootEntries() ([]entry, error) {
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
	var entries []entry
	for {
		t, err := r.u8()
		if err != nil {
			return nil, err
		}
		if t == tagEnd {
			return entries, nil
		}
		name, err := r.str()
		if err != nil {
			return nil, err
		}
		v, err := r.value(t, 1)
		if err != nil {
			return nil, fmt.Errorf("%v: %w", name, err)
		}
		entries = append(entries, entry{name, v})
	}
}

// networkWriter writes the typed values of reader in the network little-endian encoding: varint ints,
// longs and lengths, varuint32 string lengths, an empty root name.
type networkWriter struct {
	b bytes.Buffer
}

func (w *networkWriter) varuint(v uint64) {
	w.b.Write(binary.AppendUvarint(nil, v))
}

func (w *networkWriter) i32(v int32) { w.varuint(uint64(uint32(v<<1) ^ uint32(v>>31))) }

func (w *networkWriter) i64(v int64) { w.varuint(uint64(v<<1) ^ uint64(v>>63)) }

func (w *networkWriter) str(s string) {
	w.varuint(uint64(len(s)))
	w.b.WriteString(s)
}

func (w *networkWriter) root(c map[string]any) error {
	w.b.WriteByte(tagCompound)
	w.str("")
	return w.compound(c)
}

func (w *networkWriter) compound(c map[string]any) error {
	keys := make([]string, 0, len(c))
	for k := range c {
		keys = append(keys, k)
	}
	sort.Strings(keys)
	for _, k := range keys {
		typed, ok := c[k].(map[string]any)
		if !ok {
			return fmt.Errorf("%v: not a typed value", k)
		}
		t, err := tagOf(typed)
		if err != nil {
			return fmt.Errorf("%v: %w", k, err)
		}
		w.b.WriteByte(t)
		w.str(k)
		if err := w.payload(t, typed[typeNames[t]]); err != nil {
			return fmt.Errorf("%v: %w", k, err)
		}
	}
	w.b.WriteByte(tagEnd)
	return nil
}

// tagOf returns the tag type of a typed value {"type": value}.
func tagOf(typed map[string]any) (uint8, error) {
	if len(typed) != 1 {
		return 0, errors.New("not a typed value")
	}
	for t, name := range typeNames {
		if _, ok := typed[name]; ok && t != tagEnd {
			return uint8(t), nil
		}
	}
	return 0, fmt.Errorf("unknown type %v", tagName(typed))
}

func (w *networkWriter) payload(t uint8, v any) error {
	bad := fmt.Errorf("bad %v value %v", typeNames[t], v)
	switch t {
	case tagByte:
		x, ok := v.(int8)
		if !ok {
			return bad
		}
		w.b.WriteByte(byte(x))
	case tagShort:
		x, ok := v.(int16)
		if !ok {
			return bad
		}
		w.b.Write(binary.LittleEndian.AppendUint16(nil, uint16(x)))
	case tagInt:
		x, ok := v.(int32)
		if !ok {
			return bad
		}
		w.i32(x)
	case tagLong:
		x, ok := v.(int64)
		if !ok {
			return bad
		}
		w.i64(x)
	case tagFloat:
		x, ok := v.(uint32)
		if !ok {
			return bad
		}
		w.b.Write(binary.LittleEndian.AppendUint32(nil, x))
	case tagDouble:
		x, ok := v.(uint64)
		if !ok {
			return bad
		}
		w.b.Write(binary.LittleEndian.AppendUint64(nil, x))
	case tagByteArray:
		x, ok := v.([]int)
		if !ok {
			return bad
		}
		w.i32(int32(len(x)))
		for _, b := range x {
			w.b.WriteByte(byte(b))
		}
	case tagString:
		x, ok := v.(string)
		if !ok {
			return bad
		}
		w.str(x)
	case tagList:
		x, ok := v.(map[string]any)
		values, okValues := x["values"].([]any)
		name, okType := x["type"].(string)
		if !ok || !okValues || !okType {
			return bad
		}
		et := -1
		for i, n := range typeNames {
			if n == name {
				et = i
			}
		}
		if et < 0 {
			return bad
		}
		w.b.WriteByte(uint8(et))
		w.i32(int32(len(values)))
		for _, e := range values {
			typed, ok := e.(map[string]any)
			if !ok {
				return bad
			}
			if err := w.payload(uint8(et), typed[name]); err != nil {
				return err
			}
		}
	case tagCompound:
		x, ok := v.(map[string]any)
		if !ok {
			return bad
		}
		return w.compound(x)
	case tagIntArray, tagLongArray:
		x, ok := v.([]any)
		if !ok {
			return bad
		}
		w.i32(int32(len(x)))
		for _, e := range x {
			if t == tagIntArray {
				n, ok := e.(int32)
				if !ok {
					return bad
				}
				w.i32(n)
			} else {
				n, ok := e.(int64)
				if !ok {
					return bad
				}
				w.i64(n)
			}
		}
	default:
		return bad
	}
	return nil
}
