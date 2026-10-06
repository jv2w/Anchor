package main

import (
	"errors"
	"fmt"
	"math"
	"reflect"
	"time"

	"github.com/go-gl/mathgl/mgl32"
	"github.com/sandertv/gophertunnel/minecraft/protocol"
	"github.com/sandertv/gophertunnel/minecraft/protocol/packet"
)

// The game scenarios' scripts (game.go). Each one sets up its area with console commands, plays the
// feature like the official client and checks the outcome in its model of the client's state.

// maxStack is the stack size the destination search assumes for items that stack.
const maxStack = 64

// destination is the inventory slot a crafted or taken item goes to: a stack of the same item with room,
// else the first empty slot; tools always go to an empty slot.
func (g *game) destination(it protocol.ItemStack, stacks bool) (byte, error) {
	if stacks {
		for slot := byte(0); slot < 36; slot++ {
			st, ok := g.slots[slotRef{protocol.WindowIDInventory, slot}]
			if ok && !st.empty() && st.item.NetworkID == it.NetworkID && st.item.MetadataValue == it.MetadataValue &&
				int(st.item.Count)+int(it.Count) <= maxStack {
				return slot, nil
			}
		}
	}
	return g.emptySlot()
}

// craft crafts recipe r once from the crafting grid slots (one item each) like the client taking the
// result: CraftRecipe, CraftResultsDeprecated, a Consume per grid slot and a Place of the result from the
// created output into the inventory.
func (g *game) craft(name string, r recipe, grid []byte, stacks bool) error {
	return g.craftAs(name, r, grid, stacks, r.output[0])
}

// craftAs is craft with the item the CraftResultsDeprecated action names.
func (g *game) craftAs(name string, r recipe, grid []byte, stacks bool, named protocol.ItemStack) error {
	out := r.output[0]
	dst, err := g.destination(out, stacks)
	if err != nil {
		return err
	}
	id := g.requestID()
	actions := []protocol.StackRequestAction{
		&protocol.CraftRecipeStackRequestAction{RecipeNetworkID: r.networkID, NumberOfCrafts: 1},
		&protocol.CraftResultsDeprecatedStackRequestAction{ResultItems: []protocol.StackRequestItem{g.resultItem(named)}, TimesCrafted: 1},
	}
	for _, slot := range grid {
		actions = append(actions, consume(1, g.uiSlot(protocol.ContainerCraftingInput, slot)))
	}
	actions = append(actions, place(byte(out.Count), createdSlot(id), g.slotInfo(dst)))
	_, err = g.request(name, id, out, actions...)
	return err
}

// fillGrid puts one item of the named stack into each crafting grid slot, one Place request per slot like
// a player dragging items in.
func (g *game) fillGrid(name string, item string, grid []byte) error {
	for _, slot := range grid {
		src, _, ok := g.find(item)
		if !ok {
			return fmt.Errorf("%v: no %v in the inventory", name, item)
		}
		if _, err := g.request(name, g.requestID(), protocol.ItemStack{}, place(1, g.slotInfo(src), g.uiSlot(protocol.ContainerCraftingInput, slot))); err != nil {
			return err
		}
	}
	return nil
}

// playCraft: logs into planks in the 2x2 grid, two crafting tables, sticks from the recipe book
// (CraftRecipeAuto); one table placed with the crafted item's form and one with a wrong block runtime ID;
// then a wooden pickaxe in the 3x3 grid of the first table. Then the bot dives into a pool (dive).
func playCraft(g *game) error {
	if err := g.give("oak_log", 4); err != nil {
		return err
	}
	g.check("given", g.inventorySnapshot())
	planks, err := g.recipeFor("crafting_table", "minecraft:oak_planks", "minecraft:oak_log")
	if err != nil {
		return err
	}
	table, err := g.recipeFor("crafting_table", "minecraft:crafting_table", "minecraft:oak_planks", "#minecraft:planks")
	if err != nil {
		return err
	}
	sticks, err := g.recipeFor("crafting_table", "minecraft:stick", "minecraft:oak_planks", "#minecraft:planks")
	if err != nil {
		return err
	}
	pickaxe, err := g.recipeFor("crafting_table", "minecraft:wooden_pickaxe", "minecraft:oak_planks", "#minecraft:planks", "minecraft:stick")
	if err != nil {
		return err
	}
	g.check("recipes", map[string]any{
		"planks": planks.id, "table": table.id, "sticks": sticks.id, "pickaxe": pickaxe.id,
		"table_output": toJSON(reflect.ValueOf(table.output[0]), 0, true),
	})
	if err := g.openInventory(); err != nil {
		return err
	}
	logs, st, _ := g.find("minecraft:oak_log")
	if _, err := g.request("logs into the grid", g.requestID(), protocol.ItemStack{},
		place(byte(st.item.Count), g.slotInfo(logs), g.uiSlot(protocol.ContainerCraftingInput, 28))); err != nil {
		return err
	}
	for range 4 {
		if err := g.craft("craft planks", planks, []byte{28}, true); err != nil {
			return err
		}
	}
	if err := g.fillGrid("planks into the grid", "minecraft:oak_planks", []byte{28, 29, 30, 31}); err != nil {
		return err
	}
	if err := g.craft("craft a crafting table", table, []byte{28, 29, 30, 31}, false); err != nil {
		return err
	}
	// The second table names a result in CraftResultsDeprecated with the block runtime ID of stone: does
	// the server check the item form a client derives from CraftingData?
	if err := g.fillGrid("planks into the grid", "minecraft:oak_planks", []byte{28, 29, 30, 31}); err != nil {
		return err
	}
	wrong := table.output[0]
	wrong.BlockRuntimeID = int32(state("minecraft:stone", nil))
	err = g.craftAs("craft a crafting table named with a wrong block runtime ID", table, []byte{28, 29, 30, 31}, false, wrong)
	g.check("deprecated result form", map[string]any{"accepted": err == nil})
	if err != nil {
		if err := g.craft("craft a crafting table", table, []byte{28, 29, 30, 31}, false); err != nil {
			return err
		}
	}
	if err := g.autoCraft("craft sticks from the recipe book", sticks, "minecraft:oak_planks"); err != nil {
		return err
	}
	if err := g.closeWindow("close the inventory"); err != nil {
		return err
	}
	g.check("crafted", g.inventorySnapshot())

	// Place both tables: the first held with the form the client derived from CraftingData, the second with
	// the block runtime ID of stone, to see whether the server checks the held item's form.
	first, second := g.offset(2, 0, 0), g.offset(-2, 0, 0)
	tableID := state("minecraft:crafting_table", nil)
	for i, at := range []protocol.BlockPos{first, second} {
		slot, st, ok := g.find("minecraft:crafting_table")
		if !ok {
			return errors.New("no crafting table to place")
		}
		if err := g.hold(slot); err != nil {
			return err
		}
		name := "place a crafting table"
		if i == 1 {
			st.item.BlockRuntimeID = int32(state("minecraft:stone", nil))
			g.slots[slotRef{protocol.WindowIDInventory, slot}] = st
			name = "place a crafting table held with a wrong block runtime ID"
		}
		ground := protocol.BlockPos{at[0], at[1] - 1, at[2]}
		if err := g.useOnBlock(name, ground, 1, true); err != nil {
			return err
		}
		err := g.waitState("the placed table", 5*time.Second, func() bool { return g.blocks[at] == tableID })
		if err != nil && !errors.Is(err, errTimeout) {
			return err
		}
		g.check(name, map[string]any{"placed": err == nil})
		if i == 0 && err != nil {
			return err
		}
		// The server resends the player's state after a placement; let it arrive inside this step.
		if err := g.settle(time.Second); err != nil {
			return err
		}
	}
	if err := g.settle(time.Second); err != nil {
		return err
	}
	if err := g.hold(0); err != nil {
		return err
	}
	if err := g.openBlock("open the crafting table", first, protocol.ContainerTypeWorkbench); err != nil {
		return err
	}
	if err := g.fillGrid("planks into the grid", "minecraft:oak_planks", []byte{32, 33, 34}); err != nil {
		return err
	}
	if err := g.fillGrid("sticks into the grid", "minecraft:stick", []byte{36, 39}); err != nil {
		return err
	}
	if err := g.craft("craft a wooden pickaxe", pickaxe, []byte{32, 33, 34, 36, 39}, false); err != nil {
		return err
	}
	if err := g.closeWindow("close the crafting table"); err != nil {
		return err
	}
	if err := g.settle(time.Second); err != nil {
		return err
	}
	g.check("final", g.inventorySnapshot())
	if g.count("minecraft:wooden_pickaxe") != 1 {
		return fmt.Errorf("the inventory holds %v wooden pickaxes", g.count("minecraft:wooden_pickaxe"))
	}
	return g.dive()
}

// autoCraft crafts the recipe once from the recipe book (CraftRecipeAuto) with ingredients of the named item
// taken from the inventory; the result goes to the inventory. The client's exact ingredient list is not
// documented, so the variants below are tried in order and each attempt is recorded.
func (g *game) autoCraft(name string, r recipe, item string) error {
	out := r.output[0]
	var tags []protocol.ItemDescriptorCount
	total := byte(0)
	for _, in := range r.input {
		if in.Count == 0 {
			continue
		}
		tags = append(tags, in)
		total += byte(in.Count)
	}
	merged := []protocol.ItemDescriptorCount{{Descriptor: tags[0].Descriptor, Count: int32(total)}}
	named := &protocol.DefaultItemDescriptor{Name: item}
	variants := []struct {
		name        string
		ingredients []protocol.ItemDescriptorCount
		split       bool
	}{
		{"the item by name each, a consume each", []protocol.ItemDescriptorCount{{Descriptor: named, Count: 1}, {Descriptor: named, Count: 1}}, true},
		{"the item by name, one consume", []protocol.ItemDescriptorCount{{Descriptor: named, Count: int32(total)}}, false},
		{"recipe inputs, one consume", tags, false},
		{"recipe inputs, a consume each", tags, true},
		{"no ingredients, one consume", nil, false},
		{"merged recipe input, one consume", merged, false},
	}
	for _, v := range variants {
		dst, err := g.destination(out, true)
		if err != nil {
			return err
		}
		slot, _, ok := g.find(item)
		if !ok {
			return fmt.Errorf("%v: no %v", name, item)
		}
		id := g.requestID()
		actions := []protocol.StackRequestAction{
			&protocol.AutoCraftRecipeStackRequestAction{RecipeNetworkID: r.networkID, NumberOfCrafts: 1, Ingredients: v.ingredients},
			&protocol.CraftResultsDeprecatedStackRequestAction{ResultItems: []protocol.StackRequestItem{g.resultItem(out)}, TimesCrafted: 1},
		}
		if v.split {
			for range total {
				actions = append(actions, consume(1, g.slotInfo(slot)))
			}
		} else {
			actions = append(actions, consume(total, g.slotInfo(slot)))
		}
		actions = append(actions, place(byte(out.Count), createdSlot(id), g.slotInfo(dst)))
		_, err = g.request(name+" ("+v.name+")", id, out, actions...)
		g.check("auto craft variant", map[string]any{"variant": v.name, "accepted": err == nil})
		if err == nil {
			return nil
		}
	}
	return fmt.Errorf("%v: no variant was accepted", name)
}

// playFurnace: a furnace, one coal and two raw iron; fuel and input placed, the two ingots taken out.
func playFurnace(g *game) error {
	at := g.offset(2, 0, 0)
	g.setup("place the furnace")
	g.command(`setblock %v %v %v furnace ["minecraft:cardinal_direction"="west"]`, at[0], at[1], at[2])
	furnace := state("minecraft:furnace", map[string]any{"minecraft:cardinal_direction": "west"})
	if err := g.waitState("the furnace", gameWait, func() bool { return g.blocks[at] == furnace }); err != nil {
		return err
	}
	if err := g.settleSetup(); err != nil {
		return err
	}
	if err := g.give("coal", 1); err != nil {
		return err
	}
	if err := g.give("raw_iron", 2); err != nil {
		return err
	}
	if err := g.openBlock("open the furnace", at, protocol.ContainerTypeFurnace); err != nil {
		return err
	}
	coal, _, _ := g.find("minecraft:coal")
	if _, err := g.request("fuel", g.requestID(), protocol.ItemStack{}, place(1, g.slotInfo(coal), g.windowSlot(protocol.ContainerFurnaceFuel, 1))); err != nil {
		return err
	}
	iron, _, _ := g.find("minecraft:raw_iron")
	if _, err := g.request("input", g.requestID(), protocol.ItemStack{}, place(2, g.slotInfo(iron), g.windowSlot(protocol.ContainerFurnaceIngredient, 0))); err != nil {
		return err
	}
	window := uint32(g.window.WindowID)
	if err := g.waitState("two iron ingots", 45*time.Second, func() bool {
		st := g.slots[slotRef{window, 2}]
		_, input := g.slots[slotRef{window, 0}]
		return g.name(st.item) == "minecraft:iron_ingot" && st.item.Count == 2 && !input
	}); err != nil {
		return err
	}
	dst, err := g.emptySlot()
	if err != nil {
		return err
	}
	if _, err := g.request("take the ingots", g.requestID(), protocol.ItemStack{}, place(2, g.windowSlot(protocol.ContainerFurnaceResult, 2), g.slotInfo(dst))); err != nil {
		return err
	}
	if err := g.settle(2 * time.Second); err != nil {
		return err
	}
	if err := g.closeWindow("close the furnace"); err != nil {
		return err
	}
	if err := g.settle(3 * time.Second); err != nil {
		return err
	}
	g.check("final", map[string]any{"inventory": g.inventorySnapshot(), "attributes": g.attrs})
	if g.count("minecraft:iron_ingot") != 2 {
		return fmt.Errorf("the inventory holds %v iron ingots", g.count("minecraft:iron_ingot"))
	}
	return nil
}

// playEnchant: an enchanting table in a ring of bookshelves, 30 levels, an iron pickaxe and three lapis;
// the first option is applied.
func playEnchant(g *game) error {
	at := g.offset(3, 0, 0)
	x, y, z := at[0], at[1], at[2]
	g.setup("place the enchanting table and bookshelves")
	g.command("fill %v %v %v %v %v %v bookshelf", x-2, y, z-2, x+2, y+1, z+2)
	g.command("fill %v %v %v %v %v %v air", x-1, y, z-1, x+1, y+1, z+1)
	// A gap in the ring towards the player.
	g.command("fill %v %v %v %v %v %v air", x-2, y, z, x-2, y+1, z)
	g.command("setblock %v %v %v enchanting_table", x, y, z)
	tableID := state("minecraft:enchanting_table", nil)
	if err := g.waitState("the enchanting table", gameWait, func() bool { return g.blocks[at] == tableID }); err != nil {
		return err
	}
	if err := g.settleSetup(); err != nil {
		return err
	}
	g.setup("give levels")
	g.command("xp 30L %v", g.s.c.name)
	if err := g.waitState("30 levels", gameWait, func() bool { return g.attrs["minecraft:player.level"] >= 30 }); err != nil {
		return err
	}
	if err := g.settleSetup(); err != nil {
		return err
	}
	if err := g.give("iron_pickaxe", 1); err != nil {
		return err
	}
	if err := g.give("lapis_lazuli", 3); err != nil {
		return err
	}
	if err := g.openBlock("open the enchanting table", at, protocol.ContainerTypeEnchantment); err != nil {
		return err
	}
	pick, _, _ := g.find("minecraft:iron_pickaxe")
	if _, err := g.request("pickaxe into the table", g.requestID(), protocol.ItemStack{}, place(1, g.slotInfo(pick), g.uiSlot(protocol.ContainerEnchantingInput, 14))); err != nil {
		return err
	}
	lapis, _, _ := g.find("minecraft:lapis_lazuli")
	if _, err := g.request("lapis into the table", g.requestID(), protocol.ItemStack{}, place(3, g.slotInfo(lapis), g.uiSlot(protocol.ContainerEnchantingMaterial, 15))); err != nil {
		return err
	}
	if err := g.waitState("PlayerEnchantOptions", gameWait, func() bool { return len(g.options) > 0 }); err != nil {
		return err
	}
	option := g.options[0]
	g.check("options", toJSON(reflect.ValueOf(g.options), 0, true))
	input := g.slots[slotRef{protocol.WindowIDUI, 14}]
	id := g.requestID()
	result := input.item
	actions := []protocol.StackRequestAction{
		&protocol.CraftRecipeStackRequestAction{RecipeNetworkID: option.RecipeNetworkID, NumberOfCrafts: 1},
		consume(1, g.uiSlot(protocol.ContainerEnchantingInput, 14)),
		// The lapis cost is the option's index plus one; Cost is the level requirement.
		consume(1, g.uiSlot(protocol.ContainerEnchantingMaterial, 15)),
		place(1, createdSlot(id), protocol.StackRequestSlotInfo{
			Container: protocol.FullContainerName{ContainerID: protocol.ContainerEnchantingInput}, Slot: 14, StackNetworkID: id,
		}),
	}
	if _, err := g.request("enchant", id, result, actions...); err != nil {
		return err
	}
	if err := g.waitState("the level cost", gameWait, func() bool { return g.attrs["minecraft:player.level"] < 30 }); err != nil {
		return err
	}
	dst, err := g.emptySlot()
	if err != nil {
		return err
	}
	if _, err := g.request("take the pickaxe", g.requestID(), protocol.ItemStack{}, place(1, g.uiSlot(protocol.ContainerEnchantingInput, 14), g.slotInfo(dst))); err != nil {
		return err
	}
	if left := g.slots[slotRef{protocol.WindowIDUI, 15}]; !left.empty() {
		back, err := g.destination(left.item, true)
		if err != nil {
			return err
		}
		if _, err := g.request("take the lapis", g.requestID(), protocol.ItemStack{}, place(byte(left.item.Count), g.uiSlot(protocol.ContainerEnchantingMaterial, 15), g.slotInfo(back))); err != nil {
			return err
		}
	}
	if err := g.closeWindow("close the enchanting table"); err != nil {
		return err
	}
	if err := g.settle(2 * time.Second); err != nil {
		return err
	}
	g.check("final", map[string]any{"inventory": g.inventorySnapshot(), "attributes": g.attrs})
	if g.attrs["minecraft:player.level"] >= 30 {
		return errors.New("enchanting cost no levels")
	}
	return nil
}

// playCrops: farmland and a wheat crop, bone meal until it is grown, then broken by server-authoritative
// block breaking; the drops are picked up.
func playCrops(g *game) error {
	at := g.offset(1, 0, 0)
	g.setup("place the crop")
	g.command("setblock %v %v %v farmland", at[0], at[1]-1, at[2])
	g.command(`setblock %v %v %v wheat ["growth"=0]`, at[0], at[1], at[2])
	growth := func(n int32) uint32 { return state("minecraft:wheat", map[string]any{"growth": n}) }
	if err := g.waitState("the crop", gameWait, func() bool { return g.blocks[at] == growth(0) }); err != nil {
		return err
	}
	if err := g.settleSetup(); err != nil {
		return err
	}
	if err := g.give("bone_meal", 16); err != nil {
		return err
	}
	meal, _, _ := g.find("minecraft:bone_meal")
	if err := g.hold(meal); err != nil {
		return err
	}
	// How often bone meal grows the crop is random: these steps compare by their shape.
	g.random = true
	for tries := 0; g.blocks[at] != growth(7); tries++ {
		if tries == 16 {
			return errors.New("16 bone meals did not grow the crop")
		}
		before := g.blocks[at]
		if err := g.useOnBlock("bone meal", at, 1, true); err != nil {
			return err
		}
		if err := g.waitState("the crop to grow", 5*time.Second, func() bool { return g.blocks[at] != before }); err != nil {
			return err
		}
		if err := g.settle(300 * time.Millisecond); err != nil {
			return err
		}
	}
	// From here on the bone meal left and the number of drops are random too.
	g.check("grown", map[string]any{"bone_meal_left": g.count("minecraft:bone_meal")})
	if err := g.hold(1); err != nil {
		return err
	}
	g.step("break the crop", nil)
	g.input.look(blockCentre(at))
	g.input.blockActions([]protocol.PlayerBlockAction{
		{Action: protocol.PlayerActionStartBreak, BlockPos: at, Face: 1},
		{Action: protocol.PlayerActionPredictDestroyBlock, BlockPos: at, Face: 1},
	}, nil)
	air := state("minecraft:air", nil)
	if err := g.waitState("the crop to break", gameWait, func() bool { return g.blocks[at] == air }); err != nil {
		return err
	}
	if err := g.waitState("the drops", 5*time.Second, func() bool { return len(g.items) > 0 }); err != nil {
		return err
	}
	// Let the drops land and their pickup delay pass.
	if err := g.settle(1500 * time.Millisecond); err != nil {
		return err
	}
	// Server-authoritative movement corrects a walk that does not replay the client's physics: the bot is
	// teleported onto each drop.
	for range 8 {
		var rid uint64
		for id := range g.items {
			if rid == 0 || id < rid {
				rid = id
			}
		}
		if rid == 0 {
			break
		}
		g.step("pick up a drop", nil)
		at := g.positions[rid]
		if err := g.teleportTo(mgl32.Vec3{at[0], float32(math.Floor(float64(at[1]) + 0.5)), at[2]}); err != nil {
			return err
		}
		if err := g.waitState("the pickup", 5*time.Second, func() bool { _, ok := g.items[rid]; return !ok }); err != nil {
			return err
		}
	}
	g.random = false
	if err := g.settle(time.Second); err != nil {
		return err
	}
	g.check("final", g.inventorySnapshot())
	if len(g.items) > 0 || g.count("minecraft:wheat") == 0 {
		return fmt.Errorf("%v drops left, %v wheat picked up", len(g.items), g.count("minecraft:wheat"))
	}
	return nil
}

// dive: the bot stands in a glass-walled pool of still water three blocks deep until it takes drowning
// damage, then is teleported out; it records its Breathing entity flag before, under water and after.
func (g *game) dive() error {
	pool := g.offset(0, 0, -4)
	g.setup("build a pool")
	g.command("fill %v %v %v %v %v %v glass", pool[0]-2, pool[1], pool[2]-2, pool[0]+2, pool[1]+3, pool[2]+2)
	g.command("fill %v %v %v %v %v %v water", pool[0]-1, pool[1], pool[2]-1, pool[0]+1, pool[1]+2, pool[2]+1)
	water := state("minecraft:water", map[string]any{"liquid_depth": int32(0)})
	if err := g.waitState("the pool", gameWait, func() bool { return g.blocks[pool] == water }); err != nil {
		return err
	}
	if err := g.settleSetup(); err != nil {
		return err
	}
	g.step("dive", nil)
	g.check("breathing before diving", g.breathing())
	if err := g.teleportTo(blockCentre(pool).Sub(mgl32.Vec3{0, 0.5, 0})); err != nil {
		return err
	}
	if err := g.waitState("out of breath", 5*time.Second, func() bool { return !g.breathing() }); err != nil {
		return err
	}
	g.check("breathing under water", g.breathing())
	// 300 ticks of air, then drowning damage when it reaches -20.
	if err := g.waitState("drowning", 30*time.Second, func() bool { return g.attrs["minecraft:health"] < 20 }); err != nil {
		return err
	}
	g.check("drowned", map[string]any{"health": g.attrs["minecraft:health"]})
	g.step("surface", nil)
	if err := g.teleportTo(blockCentre(g.area).Sub(mgl32.Vec3{0, 0.5, 0})); err != nil {
		return err
	}
	if err := g.waitState("breathing again", 10*time.Second, g.breathing); err != nil {
		return err
	}
	g.check("breathing after surfacing", g.breathing())
	return nil
}

// playTrade: a villager with slowness (trader); the bot opens trading, puts the first payable offer's
// payment into the first ingredient slot and takes the result, opens trading again to see the updated
// offers, then summons a legacy villager and interacts with it.
func playTrade(g *game) error {
	at := g.offset(2, 0, 0)
	if err := g.give("emerald", 64); err != nil {
		return err
	}
	for _, item := range []string{"wheat", "potato", "carrot", "beetroot", "pumpkin", "melon_block"} {
		if err := g.give(item, 64); err != nil {
			return err
		}
	}
	if err := g.hold(8); err != nil {
		return err
	}
	// A villager's profession and offers are random: the trade steps compare by their shape.
	g.random = true
	villager, offer, err := g.trader(at, summonForms, true)
	if err != nil {
		return err
	}
	g.check("offer", nbtJSON(offer.raw, 0))
	if err := g.takeOffer(offer); err != nil {
		return err
	}
	if err := g.settle(3 * time.Second); err != nil {
		return err
	}
	if g.trade != nil {
		after, err := decodeOffers(g.trade.SerialisedOffers)
		g.check("offers after the trade", map[string]any{"offers": nbtJSON(after, 0), "error": fmt.Sprint(err)})
	} else {
		g.check("offers after the trade", "no UpdateTrade")
	}
	if err := g.closeWindow("close trading"); err != nil {
		return err
	}
	if err := g.settle(time.Second); err != nil {
		return err
	}
	g.check("traded", g.inventorySnapshot())

	// Open trading again: the offer list as the server keeps it after one trade.
	g.trade = nil
	if err := g.useOnEntity("open trading again", villager.EntityRuntimeID, villager.Position.Add(mgl32.Vec3{0, 1, 0})); err != nil {
		return err
	}
	if err := g.waitState("UpdateTrade", gameWait, func() bool { return g.trade != nil }); err != nil {
		return err
	}
	reopened, err := decodeOffers(g.trade.SerialisedOffers)
	if err != nil {
		return fmt.Errorf("decode the offers: %w", err)
	}
	g.check("offers on reopening", nbtJSON(reopened, 0))
	if g.window == nil {
		g.window = &packet.ContainerOpen{WindowID: g.trade.WindowID, ContainerType: g.trade.WindowType}
	}
	if err := g.closeWindow("close trading again"); err != nil {
		return err
	}
	if err := g.settle(time.Second); err != nil {
		return err
	}

	// The legacy villager: what the legacy entity name spawns and whether it opens trading.
	old, _, err := g.trader(g.offset(-2, 0, 0), legacyForms, false)
	if err != nil {
		g.check("legacy trading", map[string]any{"update_trade": false, "error": err.Error()})
		return nil
	}
	g.check("legacy villager", old.EntityType)
	g.check("legacy trading", map[string]any{"update_trade": true})
	if err := g.closeWindow("close legacy trading"); err != nil {
		return err
	}
	return g.settle(time.Second)
}

// takeOffer trades once in the open trade window, like the client: the payment into the first ingredient
// slot, then the offer's recipe with the result placed into the inventory.
func (g *game) takeOffer(o offer) error {
	src, _, ok := g.find(o.buyName)
	if !ok {
		return fmt.Errorf("no %v to pay with", o.buyName)
	}
	if _, err := g.request("payment into the trade", g.requestID(), protocol.ItemStack{},
		place(o.buyCount, g.slotInfo(src), g.uiSlot(protocol.ContainerTradeTwoIngredientOne, 4))); err != nil {
		return err
	}
	g.trade = nil
	sell := protocol.ItemStack{ItemType: protocol.ItemType{NetworkID: o.sellID}, Count: uint16(o.sellCount)}
	dst, err := g.destination(sell, true)
	if err != nil {
		return err
	}
	id := g.requestID()
	actions := []protocol.StackRequestAction{
		&protocol.CraftRecipeStackRequestAction{RecipeNetworkID: o.netID, NumberOfCrafts: 1},
		&protocol.CraftResultsDeprecatedStackRequestAction{ResultItems: []protocol.StackRequestItem{g.resultItem(sell)}, TimesCrafted: 1},
		consume(o.buyCount, g.uiSlot(protocol.ContainerTradeTwoIngredientOne, 4)),
		place(o.sellCount, createdSlot(id), g.slotInfo(dst)),
	}
	_, err = g.request("trade", id, sell, actions...)
	return err
}

// summonForms are the console forms tried for a villager; the first that makes one is used from then on.
// "villager" makes a villager_v2 in BDS 1.26.52, and the spawn event does not pick the profession.
var summonForms = []string{
	"summon villager %v %v %v 0 0 minecraft:spawn_farmer P6Farmer",
	"summon villager %v %v %v",
}

// legacyForms summon the legacy villager entity (minecraft:villager, before villager_v2).
var legacyForms = []string{"summon minecraft:villager %v %v %v"}

// trader summons villagers at the position with the first command form that works until one opens
// trading (a villager's profession is random; some have no trades), with an offer the bot can pay for if
// payable, at most four. It returns the villager and the offer, and leaves the trade window open.
func (g *game) trader(at protocol.BlockPos, forms []string, payable bool) (*packet.AddActor, offer, error) {
	for range 4 {
		var villager *packet.AddActor
		g.setup("summon a villager")
		for _, form := range forms {
			g.command(form, float32(at[0])+0.5, at[1], float32(at[2])+0.5)
			pk, err := g.until("the villager", 3*time.Second, func(pk packet.Packet) bool {
				a, ok := pk.(*packet.AddActor)
				return ok && (a.EntityType == "minecraft:villager_v2" || a.EntityType == "minecraft:villager")
			})
			if err == nil {
				villager = pk.(*packet.AddActor)
				forms = []string{form}
				g.check("villager", map[string]any{"command": form, "type": villager.EntityType})
				break
			}
		}
		if villager == nil {
			return nil, offer{}, errors.New("no summon command made a villager")
		}
		g.command("effect @e[type=!player] slowness 600 255 true")
		if err := g.settle(2 * time.Second); err != nil {
			return nil, offer{}, err
		}
		g.trade = nil
		if err := g.useOnEntity("open trading", villager.EntityRuntimeID, villager.Position.Add(mgl32.Vec3{0, 1, 0})); err != nil {
			return nil, offer{}, err
		}
		if err := g.waitState("UpdateTrade", 5*time.Second, func() bool { return g.trade != nil }); err != nil {
			g.check("no trading", fmt.Sprint(villager.EntityMetadata[protocol.EntityDataKeyVariant]))
			g.command("kill @e[type=!player]")
			continue
		}
		offers, err := decodeOffers(g.trade.SerialisedOffers)
		if err != nil {
			return nil, offer{}, fmt.Errorf("decode the offers: %w", err)
		}
		g.check("offers", nbtJSON(offers, 0))
		if g.window == nil {
			g.window = &packet.ContainerOpen{WindowID: g.trade.WindowID, ContainerType: g.trade.WindowType}
		}
		o, err := g.firstOffer(offers)
		if err == nil || !payable {
			return villager, o, nil
		}
		if err := g.closeWindow("close trading without a payable offer"); err != nil {
			return nil, offer{}, err
		}
		g.command("kill @e[type=!player]")
	}
	return nil, offer{}, errors.New("four villagers had no offer the bot can pay for")
}

// offer is one trade offer the bot can pay for.
type offer struct {
	raw       map[string]any
	netID     uint32
	buyName   string
	buyCount  byte
	sellID    int32
	sellCount byte
}

// firstOffer is the first offer without a second payment whose first payment the inventory holds.
func (g *game) firstOffer(offers map[string]any) (offer, error) {
	recipes, _ := offers["Recipes"].([]any)
	for _, r := range recipes {
		m, ok := r.(map[string]any)
		if !ok {
			continue
		}
		buyA, _ := m["buyA"].(map[string]any)
		sell, _ := m["sell"].(map[string]any)
		buyName, _ := buyA["Name"].(string)
		sellName, _ := sell["Name"].(string)
		if buyB, ok := m["buyB"].(map[string]any); ok {
			if n, _ := buyB["Name"].(string); n != "" {
				continue
			}
		}
		buyCount, err1 := nbtInt(m["buyCountA"])
		netID, err2 := nbtInt(m["netId"])
		sellCount, err3 := nbtInt(sell["Count"])
		sellID, err4 := g.itemNamed(sellName)
		if err := errors.Join(err1, err2, err3, err4); err != nil {
			continue
		}
		if buyCount <= 0 || buyCount > math.MaxUint8 || g.count(buyName) < int(buyCount) {
			continue
		}
		return offer{raw: m, netID: uint32(netID), buyName: buyName, buyCount: byte(buyCount), sellID: sellID, sellCount: byte(sellCount)}, nil
	}
	return offer{}, errors.New("no offer the bot can pay for")
}

// nbtInt reads an NBT integer of any width.
func nbtInt(v any) (int64, error) {
	switch v := v.(type) {
	case uint8:
		return int64(v), nil
	case int16:
		return int64(v), nil
	case int32:
		return int64(v), nil
	case int64:
		return v, nil
	}
	return 0, fmt.Errorf("not an NBT integer: %T", v)
}
