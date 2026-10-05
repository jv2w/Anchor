package main

import (
	"bytes"
	"compress/gzip"
	"encoding/json"
	"os"
	"path/filepath"
	"reflect"
	"strings"
	"testing"

	"github.com/sandertv/gophertunnel/minecraft/nbt"
	"github.com/sandertv/gophertunnel/minecraft/protocol"
	"github.com/sandertv/gophertunnel/minecraft/protocol/packet"
)

const plainsJSON = `{"minecraft:plains": {"id": null, "temperature": 0.8, "downfall": 0.4, "redSporeDensity": 0.0,
 "blueSporeDensity": 0.0, "ashDensity": 0.0, "whiteAshDensity": 0.0, "foliageSnow": 0.0, "depth": 0.125,
 "scale": 0.05, "mapWaterColor": {"a": 165, "r": 68, "g": 175, "b": 245}, "rain": true,
 "tags": ["animal", "plains"], "chunkGenData": null}}`

// writeCloudburst writes a data directory holding plainsJSON and a jigsaw file with the given NBT.
func writeCloudburst(t *testing.T, jigsaw map[string]any) string {
	dir := t.TempDir()
	if err := os.WriteFile(filepath.Join(dir, "stripped_biome_definitions.json"), []byte(plainsJSON), 0o644); err != nil {
		t.Fatal(err)
	}
	var buf bytes.Buffer
	zw := gzip.NewWriter(&buf)
	if err := nbt.NewEncoderWithEncoding(zw, nbt.BigEndian).Encode(jigsaw); err != nil {
		t.Fatal(err)
	}
	_ = zw.Close()
	if err := os.WriteFile(filepath.Join(dir, "jigsaw_structure_data.nbt"), buf.Bytes(), 0o644); err != nil {
		t.Fatal(err)
	}
	return dir
}

// writeWorld writes a .world.jsonl file the way the bot does.
func writeWorld(t *testing.T, pks ...packet.Packet) string {
	path := filepath.Join(t.TempDir(), "spawn.world.jsonl")
	var lines []string
	for i, pk := range pks {
		b, err := json.Marshal(map[string]any{"index": i, "name": packetName(pk), "fields": toJSON(reflect.ValueOf(pk), 0, true)})
		if err != nil {
			t.Fatal(err)
		}
		lines = append(lines, string(b))
	}
	if err := os.WriteFile(path, []byte(strings.Join(lines, "\n")+"\n"), 0o644); err != nil {
		t.Fatal(err)
	}
	return path
}

func plains(tags ...uint16) *packet.BiomeDefinitionList {
	// The string list in another order than the file's tags: indexes are resolved.
	return &packet.BiomeDefinitionList{
		StringList: []string{"plains", "minecraft:plains", "animal"},
		BiomeDefinitions: []protocol.BiomeDefinition{{
			NameIndex: 1, BiomeID: -1, Temperature: 0.8, Downfall: 0.4, Depth: 0.125, Scale: 0.05,
			MapWaterColour: int32(-1522225163), Rain: true, Tags: protocol.Option(tags),
		}},
	}
}

func TestCloudburstCheckMatchesEqualContent(t *testing.T) {
	jigsaw := map[string]any{"jigsaws": []any{map[string]any{"weight": int32(3), "name": "a"}}}
	data := writeCloudburst(t, jigsaw)
	var out bytes.Buffer
	world := writeWorld(t, plains(2, 0), &packet.JigsawStructureData{StructureData: jigsaw})
	same, err := cloudburstCommand([]string{"-world", world, "-data", data}, &out)
	if err != nil || !same {
		t.Fatalf("same %v, err %v:\n%v", same, err, out.String())
	}
	// The tags in another order, and another jigsaw value.
	other := map[string]any{"jigsaws": []any{map[string]any{"weight": int32(4), "name": "a"}}}
	world = writeWorld(t, plains(0, 2), &packet.JigsawStructureData{StructureData: other})
	out.Reset()
	same, err = cloudburstCommand([]string{"-world", world, "-data", data}, &out)
	if err != nil || same || !strings.Contains(out.String(), "biome minecraft:plains") ||
		!strings.Contains(out.String(), "JigsawStructureData differs") {
		t.Fatalf("same %v, err %v:\n%v", same, err, out.String())
	}
}

func TestCloudburstCheckFailsWithoutJigsawStructureData(t *testing.T) {
	data := writeCloudburst(t, map[string]any{"jigsaws": []any{}})
	var out bytes.Buffer
	// A capture with the biome definitions but no jigsaw data, as Anchor sent before it sent one.
	world := writeWorld(t, plains(2, 0))
	same, err := cloudburstCommand([]string{"-world", world, "-data", data}, &out)
	if err != nil || same || !strings.Contains(out.String(), "no JigsawStructureData") {
		t.Fatalf("same %v, err %v:\n%v", same, err, out.String())
	}
}
