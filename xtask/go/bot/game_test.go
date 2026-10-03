package main

import (
	"math"
	"testing"

	"github.com/go-gl/mathgl/mgl32"
	"github.com/sandertv/gophertunnel/minecraft/protocol"
	"github.com/sandertv/gophertunnel/minecraft/protocol/packet"
)

func testGame() *game {
	g := newGame(&session{}, nil)
	g.names = map[int32]string{5: "minecraft:oak_planks", 17: "minecraft:oak_log", 501: "minecraft:emerald", 502: "minecraft:bread"}
	return g
}

func TestPredictMovesItemsLikeTheClient(t *testing.T) {
	g := testGame()
	logs := protocol.ItemStack{ItemType: protocol.ItemType{NetworkID: 17}, Count: 4}
	g.slots[slotRef{protocol.WindowIDInventory, 0}] = stack{item: logs, id: 2}
	planks := protocol.ItemStack{ItemType: protocol.ItemType{NetworkID: 5}, Count: 4}
	predicted := g.predict([]protocol.StackRequestAction{
		place(3, g.slotInfo(0), g.uiSlot(protocol.ContainerCraftingInput, 28)),
		consume(1, g.uiSlot(protocol.ContainerCraftingInput, 28)),
		place(4, createdSlot(-1), g.slotInfo(1)),
	}, planks)
	if got := predicted[slotRef{protocol.WindowIDInventory, 0}].item.Count; got != 1 {
		t.Fatalf("source slot holds %v logs, want 1", got)
	}
	if got := predicted[slotRef{protocol.WindowIDUI, 28}].item; got.NetworkID != 17 || got.Count != 2 {
		t.Fatalf("grid slot holds %+v, want 2 logs", got)
	}
	if got := predicted[slotRef{protocol.WindowIDInventory, 1}].item; got.NetworkID != 5 || got.Count != 4 {
		t.Fatalf("result slot holds %+v, want 4 planks", got)
	}
	if g.slots[slotRef{protocol.WindowIDInventory, 0}].item.Count != 4 {
		t.Fatal("predict changed the model")
	}
}

func TestInventorySlotsUseTheOpenWindowsContainers(t *testing.T) {
	g := testGame()
	if c := g.slotInfo(3).Container.ContainerID; c != protocol.ContainerHotBar {
		t.Fatalf("hotbar slot without a window: container %v", c)
	}
	if c := g.slotInfo(20).Container.ContainerID; c != protocol.ContainerInventory {
		t.Fatalf("inventory slot without a window: container %v", c)
	}
	g.window = &packet.ContainerOpen{WindowID: 2, ContainerType: 0xff}
	if c := g.slotInfo(3).Container.ContainerID; c != protocol.ContainerCombinedHotBarAndInventory {
		t.Fatalf("slot with a window open: container %v", c)
	}
}

func TestFirstOfferSkipsSecondPaymentsAndWhatTheBotLacks(t *testing.T) {
	g := testGame()
	g.slots[slotRef{protocol.WindowIDInventory, 0}] = stack{item: protocol.ItemStack{ItemType: protocol.ItemType{NetworkID: 501}, Count: 10}, id: 1}
	item := func(name string, count uint8) map[string]any { return map[string]any{"Name": name, "Count": count} }
	offers := map[string]any{"Recipes": []any{
		map[string]any{"buyA": item("minecraft:emerald", 1), "buyB": item("minecraft:emerald", 1), "buyCountA": int32(1), "sell": item("minecraft:bread", 6), "netId": int32(7)},
		map[string]any{"buyA": item("minecraft:emerald", 20), "buyCountA": int32(20), "sell": item("minecraft:bread", 6), "netId": int32(8)},
		map[string]any{"buyA": item("minecraft:emerald", 2), "buyB": item("", 0), "buyCountA": int32(2), "sell": item("minecraft:bread", 6), "netId": int32(9)},
	}}
	o, err := g.firstOffer(offers)
	if err != nil {
		t.Fatal(err)
	}
	if o.netID != 9 || o.buyCount != 2 || o.sellCount != 6 || o.sellID != 502 {
		t.Fatalf("offer %+v, want netId 9", o)
	}
}

func TestLookDirectionPointsAtTheTarget(t *testing.T) {
	l := &inputLoop{pos: mgl32.Vec3{0.5, 1.62, 0.5}}
	target := mgl32.Vec3{2.5, 0, -3.5}
	l.look(target)
	d := direction(l.pitch, l.yaw)
	want := target.Sub(l.pos).Normalize()
	if d.Sub(want).Len() > 1e-4 {
		t.Fatalf("direction %v, want %v (pitch %v yaw %v)", d, want, l.pitch, l.yaw)
	}
	if math.Abs(float64(d.Len())-1) > 1e-5 {
		t.Fatalf("direction %v is not a unit vector", d)
	}
}

func TestFeedHandsPacketsOverInOrderAndEnds(t *testing.T) {
	f := newFeed()
	f.push(&packet.Text{Message: "a"}, 0)
	f.push(&packet.Text{Message: "b"}, 1)
	pks, closed := f.take()
	if closed || len(pks) != 2 || pks[0].index != 0 || pks[1].index != 1 {
		t.Fatalf("took %v packets, closed %v", len(pks), closed)
	}
	f.close()
	f.push(&packet.Text{}, 2)
	if pks, closed := f.take(); !closed || len(pks) != 0 {
		t.Fatalf("after close: %v packets, closed %v", len(pks), closed)
	}
}

func TestRecipesAreFoundByOutputAndIngredients(t *testing.T) {
	g := testGame()
	planks := protocol.ItemStack{ItemType: protocol.ItemType{NetworkID: 5}, Count: 4}
	in := func(d protocol.ItemDescriptor) []protocol.ItemDescriptorCount {
		return []protocol.ItemDescriptorCount{{Descriptor: d, Count: 1}, {Count: 0}}
	}
	g.craftingData(&packet.CraftingData{
		ShapedRecipes: []protocol.ShapedRecipe{
			{RecipeID: "b", RecipeNetworkID: 9, Block: "crafting_table", Input: in(&protocol.DefaultItemDescriptor{Name: "minecraft:oak_log"}), Output: []protocol.ItemStack{planks}},
			{RecipeID: "a", RecipeNetworkID: 7, Block: "crafting_table", Input: in(&protocol.DefaultItemDescriptor{Name: "minecraft:stripped_oak_log"}), Output: []protocol.ItemStack{planks}},
			{RecipeID: "c", RecipeNetworkID: 3, Block: "stonecutter", Input: in(&protocol.DefaultItemDescriptor{Name: "minecraft:oak_log"}), Output: []protocol.ItemStack{planks}},
			{RecipeID: "d", RecipeNetworkID: 11, Block: "crafting_table", Input: in(&protocol.ItemTagItemDescriptor{Tag: "minecraft:logs"}), Output: []protocol.ItemStack{planks}},
		},
	})
	r, err := g.recipeFor("crafting_table", "minecraft:oak_planks", "minecraft:oak_log")
	if err != nil || r.id != "b" {
		t.Fatalf("found %+v, %v; want recipe b", r, err)
	}
	if r, err := g.recipeFor("crafting_table", "minecraft:oak_planks", "#minecraft:logs"); err != nil || r.id != "d" {
		t.Fatalf("found %+v, %v; want recipe d", r, err)
	}
	if _, err := g.recipeFor("crafting_table", "minecraft:oak_planks", "minecraft:dirt"); err == nil {
		t.Fatal("found a recipe from dirt")
	}
}

func TestNBTIntegersOfEveryWidthAreRead(t *testing.T) {
	for _, v := range []any{uint8(3), int16(3), int32(3), int64(3)} {
		if n, err := nbtInt(v); err != nil || n != 3 {
			t.Fatalf("nbtInt(%T) = %v, %v", v, n, err)
		}
	}
	if _, err := nbtInt("3"); err == nil {
		t.Fatal("a string was read as an integer")
	}
}
