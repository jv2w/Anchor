package main

import (
	"encoding/json"
	"math"
	"os"
	"path/filepath"
	"reflect"
	"testing"

	"github.com/sandertv/gophertunnel/minecraft/protocol"
	"github.com/sandertv/gophertunnel/minecraft/protocol/packet"
)

func TestCommandSummariesNameParameterTypesAndAliases(t *testing.T) {
	pk := &packet.AvailableCommands{
		EnumValues: []string{"tp", "teleport", "set", "add"},
		Suffixes:   []string{"L"},
		Enums: []protocol.CommandEnum{
			{Type: "TeleportAliases", ValueIndices: []uint32{0, 1}},
			{Type: "TimeMode", ValueIndices: []uint32{2, 3}},
		},
		DynamicEnums: []protocol.DynamicEnum{{Type: "Tags"}},
		Commands: []protocol.Command{
			{Name: "tp", AliasesOffset: 0, Overloads: []protocol.CommandOverload{{Parameters: []protocol.CommandParameter{
				{Name: "victim", Type: protocol.CommandArgValid | protocol.CommandArgTypeTarget},
				{Name: "destination", Type: protocol.CommandArgValid | protocol.CommandArgTypePosition, Optional: true},
			}}}},
			{Name: "time", AliasesOffset: math.MaxUint32, Overloads: []protocol.CommandOverload{{Parameters: []protocol.CommandParameter{
				{Name: "mode", Type: protocol.CommandArgValid | protocol.CommandArgEnum | 1},
				{Name: "tag", Type: protocol.CommandArgValid | protocol.CommandArgSoftEnum | 0},
				{Name: "x", Type: 2},
				{Name: "levels", Type: protocol.CommandArgSuffixed | 0},
			}}}},
		},
	}
	got, err := json.Marshal(commandsSummary(pk))
	if err != nil {
		t.Fatal(err)
	}
	var summary []map[string]any
	if err := json.Unmarshal(got, &summary); err != nil {
		t.Fatal(err)
	}
	if !reflect.DeepEqual(summary[0]["aliases"], []any{"tp", "teleport"}) || summary[1]["aliases"] != nil {
		t.Fatalf("aliases %v %v", summary[0]["aliases"], summary[1]["aliases"])
	}
	types := func(i int) []string {
		var out []string
		for _, p := range summary[i]["overloads"].([]any)[0].(map[string]any)["params"].([]any) {
			out = append(out, p.(map[string]any)["type"].(string))
		}
		return out
	}
	if want := []string{"target", "position"}; !reflect.DeepEqual(types(0), want) {
		t.Fatalf("tp types %v", types(0))
	}
	if want := []string{"enum:TimeMode", "softenum:Tags", "type:2!invalid", "suffix:L"}; !reflect.DeepEqual(types(1), want) {
		t.Fatalf("time types %v", types(1))
	}
}

func TestCustomFormsAreFilledPerElement(t *testing.T) {
	var data map[string]any
	form := `{"type":"custom_form","content":[{"type":"label"},{"type":"input"},{"type":"toggle"},
		{"type":"slider","min":0,"max":10},{"type":"dropdown"},{"type":"step_slider"},{"type":"divider"}]}`
	if err := json.Unmarshal([]byte(form), &data); err != nil {
		t.Fatal(err)
	}
	want := []any{nil, "typed", true, float64(10), 1, 1, nil}
	if got := fillForm(data); !reflect.DeepEqual(got, want) {
		t.Fatalf("answer %v", got)
	}
}

func TestScriptsReadTheirActionsAndRefuseUnknownOnes(t *testing.T) {
	dir := t.TempDir()
	good := filepath.Join(dir, "good.txt")
	text := "# join the lobby\n\nchat hello there\ncommand /spawn\nexpect-form Lobby\nanswer-form 1\nexpect-score sidebar Coins\nexpect-item minecraft:bow 1\nexpect-position 0.5 -60 0.5\n"
	if err := os.WriteFile(good, []byte(text), 0o600); err != nil {
		t.Fatal(err)
	}
	actions, err := readScript(good)
	if err != nil {
		t.Fatal(err)
	}
	want := []scriptAction{
		{verb: "chat", arg: "hello there", line: 3},
		{verb: "command", arg: "/spawn", line: 4},
		{verb: "expect-form", arg: "Lobby", line: 5},
		{verb: "answer-form", arg: "1", line: 6},
		{verb: "expect-score", arg: "sidebar Coins", line: 7},
		{verb: "expect-item", arg: "minecraft:bow 1", line: 8},
		{verb: "expect-position", arg: "0.5 -60 0.5", line: 9},
	}
	if !reflect.DeepEqual(actions, want) {
		t.Fatalf("actions %+v", actions)
	}
	bad := filepath.Join(dir, "bad.txt")
	if err := os.WriteFile(bad, []byte("chat hi\nfly away\n"), 0o600); err != nil {
		t.Fatal(err)
	}
	if _, err := readScript(bad); err == nil {
		t.Fatal("an unknown action was accepted")
	}
}
