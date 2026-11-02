package main

import (
	"bufio"
	"bytes"
	"compress/gzip"
	"encoding/json"
	"errors"
	"flag"
	"fmt"
	"io"
	"os"
	"path/filepath"
	"reflect"
	"sort"
	"strconv"

	"github.com/sandertv/gophertunnel/minecraft/nbt"
)

// worlddataCommand compares the world packets of a capture with the reference data:
//
//	bot worlddata -world CAPTURE.world.jsonl -data DIR
//
// The first BiomeDefinitionList must hold the biomes of DIR/stripped_biome_definitions.json (the definitions
// without chunk generation data), compared by biome name with string indexes resolved, so neither the biome
// order nor the string list order matters. The first JigsawStructureData must equal DIR/jigsaw_structure_data.nbt
// (gzip, big-endian NBT) value for value, with its top-level definition lists in identifier order
// (sortByIdentifier). It prints one line per difference, then SAME or DIFFERENT, and reports whether both matched.
func worlddataCommand(args []string, out io.Writer) (bool, error) {
	fs := flag.NewFlagSet("bot worlddata", flag.ContinueOnError)
	world := fs.String("world", "", "the capture's .world.jsonl file")
	data := fs.String("data", "", "the reference data checkout")
	if err := fs.Parse(args); err != nil {
		return false, err
	}
	if fs.NArg() != 0 || *world == "" || *data == "" {
		return false, errors.New("usage: bot worlddata -world FILE -data DIR")
	}
	packets, err := firstPackets(*world, "BiomeDefinitionList", "JigsawStructureData")
	if err != nil {
		return false, err
	}
	var differences []string
	if biomes, ok := packets["BiomeDefinitionList"]; ok {
		d, err := compareBiomes(biomes, filepath.Join(*data, "stripped_biome_definitions.json"))
		if err != nil {
			return false, err
		}
		differences = append(differences, d...)
	} else {
		differences = append(differences, "no BiomeDefinitionList")
	}
	if jigsaw, ok := packets["JigsawStructureData"]; ok {
		d, err := compareJigsaw(jigsaw, filepath.Join(*data, "jigsaw_structure_data.nbt"))
		if err != nil {
			return false, err
		}
		differences = append(differences, d...)
	} else {
		differences = append(differences, "no JigsawStructureData")
	}
	for _, d := range differences {
		fmt.Fprintln(out, d)
	}
	same := len(differences) == 0
	if same {
		fmt.Fprintln(out, "SAME")
	} else {
		fmt.Fprintln(out, "DIFFERENT")
	}
	return same, nil
}

// firstPackets reads the fields of the first packet of each name from a .world.jsonl file, with numbers kept
// as json.Number.
func firstPackets(path string, names ...string) (map[string]any, error) {
	f, err := os.Open(path)
	if err != nil {
		return nil, err
	}
	defer f.Close()
	found := map[string]any{}
	sc := bufio.NewScanner(f)
	sc.Buffer(nil, 64<<20)
	for sc.Scan() {
		var line struct {
			Name   string
			Fields json.RawMessage
		}
		if err := json.Unmarshal(sc.Bytes(), &line); err != nil {
			return nil, fmt.Errorf("%v: %w", path, err)
		}
		if _, ok := found[line.Name]; ok {
			continue
		}
		for _, n := range names {
			if n == line.Name {
				v, err := decodeJSON(line.Fields)
				if err != nil {
					return nil, fmt.Errorf("%v: %w", path, err)
				}
				found[n] = v
			}
		}
	}
	return found, sc.Err()
}

func decodeJSON(b []byte) (any, error) {
	d := json.NewDecoder(bytes.NewReader(b))
	d.UseNumber()
	var v any
	err := d.Decode(&v)
	return v, err
}

// biome is the content of one biome definition that both the packet and the reference file hold.
type biome struct {
	ID                                        *int64
	Temperature, Downfall, FoliageSnow, Depth float32
	Scale                                     float32
	WaterA, WaterR, WaterG, WaterB            int64
	Rain                                      bool
	Tags                                      []string
	HasTags, HasChunkGeneration               bool
	ExtraNonZero                              []string
}

// packetBiomes reads a BiomeDefinitionList's fields (gophertunnel names) into biomes by name.
func packetBiomes(fields any) (map[string]biome, error) {
	m, _ := fields.(map[string]any)
	list, _ := m["StringList"].([]any)
	str := func(index any) (string, error) {
		i, err := integer(index)
		if err != nil || i < 0 || i >= int64(len(list)) {
			return "", fmt.Errorf("string index %v of %v strings", index, len(list))
		}
		s, _ := list[i].(string)
		return s, nil
	}
	out := map[string]biome{}
	defs, _ := m["BiomeDefinitions"].([]any)
	for _, d := range defs {
		d, _ := d.(map[string]any)
		name, err := str(d["NameIndex"])
		if err != nil {
			return nil, err
		}
		var b biome
		if id, err := integer(d["BiomeID"]); err != nil {
			return nil, err
		} else if id != -1 {
			b.ID = &id
		}
		for key, dst := range map[string]*float32{
			"Temperature": &b.Temperature, "Downfall": &b.Downfall, "FoliageSnow": &b.FoliageSnow,
			"Depth": &b.Depth, "Scale": &b.Scale,
		} {
			if *dst, err = float(d[key]); err != nil {
				return nil, fmt.Errorf("%v %v: %w", name, key, err)
			}
		}
		colour, err := integer(d["MapWaterColour"])
		if err != nil {
			return nil, err
		}
		c := uint32(colour)
		b.WaterA, b.WaterR, b.WaterG, b.WaterB = int64(c>>24), int64(c>>16&0xff), int64(c>>8&0xff), int64(c&0xff)
		b.Rain, _ = d["Rain"].(bool)
		if tags, ok := d["Tags"].([]any); ok {
			b.HasTags = true
			for _, t := range tags {
				s, err := str(t)
				if err != nil {
					return nil, err
				}
				b.Tags = append(b.Tags, s)
			}
		}
		b.HasChunkGeneration = d["ChunkGeneration"] != nil
		if _, ok := out[name]; ok {
			return nil, fmt.Errorf("biome %v twice", name)
		}
		out[name] = b
	}
	return out, nil
}

// worlddataBiomes reads the reference biome definition file. Its density fields have no packet field; a
// non-zero one is reported.
func worlddataBiomes(path string) (map[string]biome, error) {
	raw, err := os.ReadFile(path)
	if err != nil {
		return nil, err
	}
	v, err := decodeJSON(raw)
	if err != nil {
		return nil, fmt.Errorf("%v: %w", path, err)
	}
	all, _ := v.(map[string]any)
	out := map[string]biome{}
	for name, d := range all {
		d, _ := d.(map[string]any)
		var b biome
		if d["id"] != nil {
			id, err := integer(d["id"])
			if err != nil {
				return nil, err
			}
			b.ID = &id
		}
		for key, dst := range map[string]*float32{
			"temperature": &b.Temperature, "downfall": &b.Downfall, "foliageSnow": &b.FoliageSnow,
			"depth": &b.Depth, "scale": &b.Scale,
		} {
			if *dst, err = float(d[key]); err != nil {
				return nil, fmt.Errorf("%v %v: %w", name, key, err)
			}
		}
		colour, _ := d["mapWaterColor"].(map[string]any)
		for key, dst := range map[string]*int64{"a": &b.WaterA, "r": &b.WaterR, "g": &b.WaterG, "b": &b.WaterB} {
			if *dst, err = integer(colour[key]); err != nil {
				return nil, fmt.Errorf("%v mapWaterColor: %w", name, err)
			}
		}
		b.Rain, _ = d["rain"].(bool)
		if tags, ok := d["tags"].([]any); ok {
			b.HasTags = true
			for _, t := range tags {
				s, _ := t.(string)
				b.Tags = append(b.Tags, s)
			}
		}
		b.HasChunkGeneration = d["chunkGenData"] != nil
		for _, key := range []string{"redSporeDensity", "blueSporeDensity", "ashDensity", "whiteAshDensity"} {
			if f, err := float(d[key]); err != nil || f != 0 {
				b.ExtraNonZero = append(b.ExtraNonZero, key)
			}
		}
		out[name] = b
	}
	return out, nil
}

// compareBiomes compares a BiomeDefinitionList's fields with the reference file.
func compareBiomes(fields any, path string) ([]string, error) {
	got, err := packetBiomes(fields)
	if err != nil {
		return nil, fmt.Errorf("BiomeDefinitionList: %w", err)
	}
	want, err := worlddataBiomes(path)
	if err != nil {
		return nil, err
	}
	names := map[string]bool{}
	for n := range got {
		names[n] = true
	}
	for n := range want {
		names[n] = true
	}
	sorted := make([]string, 0, len(names))
	for n := range names {
		sorted = append(sorted, n)
	}
	sort.Strings(sorted)
	var differences []string
	for _, n := range sorted {
		g, inGot := got[n]
		w, inWant := want[n]
		switch {
		case !inGot:
			differences = append(differences, fmt.Sprintf("biome %v: only in the reference", n))
		case !inWant:
			differences = append(differences, fmt.Sprintf("biome %v: only in the capture", n))
		case len(w.ExtraNonZero) > 0:
			differences = append(differences, fmt.Sprintf("biome %v: the reference has non-zero %v", n, w.ExtraNonZero))
		case !reflect.DeepEqual(g, w):
			differences = append(differences, fmt.Sprintf("biome %v: capture %+v != reference %+v", n, g, w))
		}
	}
	return differences, nil
}

// compareJigsaw compares a JigsawStructureData's fields with the reference NBT file, both as typed NBT JSON.
func compareJigsaw(fields any, path string) ([]string, error) {
	gz, err := os.ReadFile(path)
	if err != nil {
		return nil, err
	}
	zr, err := gzip.NewReader(bytes.NewReader(gz))
	if err != nil {
		return nil, fmt.Errorf("%v: %w", path, err)
	}
	var root map[string]any
	if err := nbt.NewDecoderWithEncoding(zr, nbt.BigEndian).Decode(&root); err != nil {
		return nil, fmt.Errorf("%v: %w", path, err)
	}
	// The same typed JSON the capture holds, read back with json.Number like the capture.
	b, err := json.Marshal(nbtJSON(root, 0))
	if err != nil {
		return nil, err
	}
	want, err := decodeJSON(b)
	if err != nil {
		return nil, err
	}
	m, _ := fields.(map[string]any)
	got := m["StructureData"]
	sortByIdentifier(got)
	sortByIdentifier(want)
	if !reflect.DeepEqual(got, want) {
		return []string{fmt.Sprintf("JigsawStructureData differs from %v (%v vs %v JSON bytes)",
			filepath.Base(path), len(mustJSON(got)), len(b))}, nil
	}
	return nil, nil
}

// sortByIdentifier sorts each top-level list of a typed-JSON StructureData compound (jigsaws, processors,
// structure_sets, template_pools) by its entries' description.identifier. Each list is a set of definitions
// that the data refers to by identifier (start_pool, structure), and the official server's Linux build sends template_pools
// in another order than its Windows build (xtask/data/p6/facts.md). Lists inside an entry keep their order.
func sortByIdentifier(data any) {
	top, _ := at(data, "compound").(map[string]any)
	for _, list := range top {
		entries, _ := at(list, "list").([]any)
		sort.SliceStable(entries, func(i, j int) bool {
			id := func(entry any) string {
				s, _ := at(entry, "compound", "description", "compound", "identifier", "string").(string)
				return s
			}
			return id(entries[i]) < id(entries[j])
		})
	}
}

// at follows keys through nested JSON objects; it is nil where one is missing.
func at(v any, keys ...string) any {
	for _, k := range keys {
		m, _ := v.(map[string]any)
		v = m[k]
	}
	return v
}

func mustJSON(v any) []byte {
	b, _ := json.Marshal(v)
	return b
}

// integer reads a whole JSON number.
func integer(v any) (int64, error) {
	n, ok := v.(json.Number)
	if !ok {
		return 0, fmt.Errorf("%v is not a number", v)
	}
	return n.Int64()
}

// float reads a JSON number as a float32, the packet's precision.
func float(v any) (float32, error) {
	n, ok := v.(json.Number)
	if !ok {
		return 0, fmt.Errorf("%v is not a number", v)
	}
	f, err := strconv.ParseFloat(n.String(), 32)
	return float32(f), err
}
