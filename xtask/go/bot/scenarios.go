package main

import (
	"errors"
	"fmt"
	"math"
	"time"

	"github.com/go-gl/mathgl/mgl32"
	"github.com/sandertv/gophertunnel/minecraft/protocol"
	"github.com/sandertv/gophertunnel/minecraft/protocol/packet"
)

// The game scenarios' scripts (game.go). Each one sets up its area with console commands, plays the
// feature like the official client and checks the outcome in its model of the client's state.

// maxStack is the stack size the destination search assumes for items that stack.
const maxStack = 64

// destination is the inventory slot a bought or taken item goes to: a stack of the same item with room,
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
	// A villager's offers and XP orbs are random: the trade steps compare by their shape.
	g.random = true
	villager, offer, err := g.trader(at, farmerSummon, true)
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
	old, _, err := g.trader(g.offset(-2, 0, 0), legacySummon, false)
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
// slot, then the offer's trade request with the result placed into the inventory.
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

// farmerSummon summons an adult farmer villager (a villager_v2 in the official server 1.26.60). The summon
// event replaces the random spawn event, and minecraft:become_farmer is the villager_v2 event that sets one
// profession: minecraft:spawn_farmer picks one of five professions at random, and a fisherman's first
// tier may hold no offer the bot can pay for.
const farmerSummon = "summon villager %v %v %v 0 0 minecraft:become_farmer P6Farmer"

// legacySummon summons the legacy villager entity name (minecraft:villager, before villager_v2) as an
// adult farmer: without the event its spawn event makes a baby or a nitwit at times, which do not trade.
const legacySummon = "summon minecraft:villager %v %v %v 0 0 minecraft:become_farmer"

// trader summons a villager at the position with the summon command and opens trading. It returns the
// villager and, if payable, the first offer the bot can pay for, and leaves the trade window open.
func (g *game) trader(at protocol.BlockPos, summon string, payable bool) (*packet.AddActor, offer, error) {
	g.setup("summon a villager")
	g.command(summon, float32(at[0])+0.5, at[1], float32(at[2])+0.5)
	pk, err := g.until("the villager", 3*time.Second, func(pk packet.Packet) bool {
		a, ok := pk.(*packet.AddActor)
		return ok && (a.EntityType == "minecraft:villager_v2" || a.EntityType == "minecraft:villager")
	})
	if err != nil {
		return nil, offer{}, err
	}
	villager := pk.(*packet.AddActor)
	g.check("villager", map[string]any{"command": summon, "type": villager.EntityType})
	g.command("effect @e[type=!player] slowness 600 255 true")
	if err := g.settle(2 * time.Second); err != nil {
		return nil, offer{}, err
	}
	g.trade = nil
	if err := g.useOnEntity("open trading", villager.EntityRuntimeID, villager.Position.Add(mgl32.Vec3{0, 1, 0})); err != nil {
		return nil, offer{}, err
	}
	if err := g.waitState("UpdateTrade", 5*time.Second, func() bool { return g.trade != nil }); err != nil {
		return nil, offer{}, fmt.Errorf("villager variant %v: %w", villager.EntityMetadata[protocol.EntityDataKeyVariant], err)
	}
	offers, err := decodeOffers(g.trade.SerialisedOffers)
	if err != nil {
		return nil, offer{}, fmt.Errorf("decode the offers: %w", err)
	}
	g.check("offers", nbtJSON(offers, 0))
	if g.window == nil {
		g.window = &packet.ContainerOpen{WindowID: g.trade.WindowID, ContainerType: g.trade.WindowType}
	}
	if !payable {
		return villager, offer{}, nil
	}
	o, err := g.firstOffer(offers, g.trade.TradeTier)
	if err != nil {
		return nil, offer{}, err
	}
	return villager, o, nil
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

// firstOffer is the first offer unlocked at the villager's tier (UpdateTrade TradeTier), without a second
// payment, whose first payment the inventory holds. The client cannot take a locked offer; the official
// server 1.26.52 accepts one anyway, and its trader XP raises the villager's tier.
func (g *game) firstOffer(offers map[string]any, tier int32) (offer, error) {
	list, _ := offers["Recipes"].([]any)
	for _, r := range list {
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
		offerTier, err5 := nbtInt(m["tier"])
		if err := errors.Join(err1, err2, err3, err4, err5); err != nil || offerTier > int64(tier) {
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
