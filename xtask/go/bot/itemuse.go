package main

import (
	"errors"
	"fmt"
	"reflect"
	"time"

	"github.com/go-gl/mathgl/mgl32"
	"github.com/sandertv/gophertunnel/minecraft/protocol"
	"github.com/sandertv/gophertunnel/minecraft/protocol/packet"
)

// The itemuse scenario uses items in the air the way the official client does (M9 follow-up U5). An item
// with a use animation (a bow, a trident, a potion to drink) starts with an InventoryTransaction UseItem
// ClickAir and, in the PlayerAuthInput of the same tick, the StartUsingItem input flag; the server answers with
// the UsingItem entity flag (bedrock-protocol-docs changelog 776: true if it agrees). A bow or trident ends with
// an InventoryTransaction ReleaseItem; a drink ends with a second ClickAir once its time has passed. A thrown
// item is a ClickAir alone.
//
//	bow      drawn for one second and released: the UsingItem flag while drawing, the arrow
//	trident  drawn for one second and released: the flag
//	drink    a night vision potion drunk: the flag, the effect and the bottle left
//	snowball thrown: the snowball
func init() {
	gameScenarios["itemuse"] = playItemUse
	gameAreas["itemuse"] = protocol.BlockPos{-20, -60, -4}
}

// nightVision is the item metadata of the night vision potion (3 minutes) on both servers.
const nightVision = 5

// usingItem reports the bot's own UsingItem entity flag.
func (g *game) usingItem() bool {
	return g.flags&(1<<protocol.EntityDataFlagUsingItem) != 0
}

// startUsing starts using the held item like the official client: the ClickAir transaction, and the
// StartUsingItem flag in the next PlayerAuthInput.
func (g *game) startUsing(name string, at mgl32.Vec3) error {
	if err := g.lookAt(at); err != nil {
		return err
	}
	st := g.slots[slotRef{protocol.WindowIDInventory, g.held}]
	data := &protocol.UseItemTransactionData{
		ActionType: protocol.UseItemActionClickAir, TriggerType: protocol.TriggerTypePlayerInput,
		HotBarSlot: int32(g.held), HeldItem: st.instance(), Position: g.pos, BlockFace: -1,
		ClientPrediction: protocol.ClientPredictionSuccess,
	}
	g.step(name, map[string]any{"transaction": toJSON(reflect.ValueOf(data), 0, true), "start using": true})
	if err := g.s.send(&packet.InventoryTransaction{TransactionData: data}); err != nil {
		return err
	}
	g.input.startUsing()
	return nil
}

// giveMeta gives one item with metadata and waits until it is in the inventory.
func (g *game) giveMeta(name string, meta int) error {
	g.setup(fmt.Sprintf("give %v %v", name, meta))
	have := g.count("minecraft:" + name)
	g.command("give %v %v 1 %v", g.s.c.name, name, meta)
	if err := g.waitState("give "+name, gameWait, func() bool { return g.count("minecraft:"+name) > have }); err != nil {
		return err
	}
	return g.settleSetup()
}

// holdItem holds the first stack of the item.
func (g *game) holdItem(name string) error {
	slot, _, ok := g.find("minecraft:" + name)
	if !ok {
		return fmt.Errorf("no %v in the inventory", name)
	}
	return g.hold(slot)
}

// collect gathers the packets of d that keep returns a fact for.
func (g *game) collect(d time.Duration, keep func(packet.Packet) any) ([]any, error) {
	var out []any
	end := time.Now().Add(d)
	for left := time.Until(end); left > 0; left = time.Until(end) {
		pk, err := g.next(left)
		if errors.Is(err, errTimeout) {
			break
		}
		if err != nil {
			return nil, err
		}
		if v := keep(pk); v != nil {
			out = append(out, v)
		}
	}
	return out, nil
}

// effectDuration keeps the duration of an added night vision effect of the bot.
func (g *game) effectDuration(pk packet.Packet) any {
	e, ok := pk.(*packet.MobEffect)
	if !ok || e.EntityRuntimeID != g.runtimeID || e.EffectType != packet.EffectNightVision || e.Operation == packet.MobEffectRemove {
		return nil
	}
	return e.Duration
}

// playItemUse is the itemuse scenario.
func playItemUse(g *game) error {
	g.setup("resistance")
	g.command("effect %v resistance 600 255 true", g.s.c.name)
	if err := g.settleSetup(); err != nil {
		return err
	}
	for _, item := range []struct {
		name  string
		count int
	}{{"bow", 1}, {"arrow", 16}, {"trident", 1}, {"snowball", 4}} {
		if err := g.give(item.name, item.count); err != nil {
			return err
		}
	}
	ahead := g.pos.Add(mgl32.Vec3{0, 0, 8})

	// The bow.
	if err := g.holdItem("bow"); err != nil {
		return err
	}
	if err := g.startUsing("draw the bow", ahead); err != nil {
		return err
	}
	if err := g.settle(time.Second); err != nil {
		return err
	}
	g.check("drawing the bow", g.usingItem())
	if err := g.releaseItem("release the bow"); err != nil {
		return err
	}
	_, err := g.actorAdded("minecraft:arrow", gameWait)
	g.check("arrow shot", err == nil)
	if err != nil && !errors.Is(err, errTimeout) {
		return err
	}
	if err := g.settle(time.Second); err != nil {
		return err
	}
	g.check("bow released", !g.usingItem())

	// The trident.
	if err := g.holdItem("trident"); err != nil {
		return err
	}
	if err := g.startUsing("draw the trident", ahead); err != nil {
		return err
	}
	if err := g.settle(time.Second); err != nil {
		return err
	}
	g.check("drawing the trident", g.usingItem())
	if err := g.releaseItem("release the trident"); err != nil {
		return err
	}
	if err := g.settle(time.Second); err != nil {
		return err
	}
	g.check("trident released", !g.usingItem())

	// A potion drunk: the second ClickAir after the drinking time finishes it.
	if err := g.giveMeta("potion", nightVision); err != nil {
		return err
	}
	if err := g.holdItem("potion"); err != nil {
		return err
	}
	if err := g.startUsing("drink the potion", ahead); err != nil {
		return err
	}
	// BDS finishes the drink by its own clock, PMMP at the second ClickAir: the effect of either counts.
	drunk, err := g.collect(500*time.Millisecond, g.effectDuration)
	if err != nil {
		return err
	}
	g.check("drinking", g.usingItem())
	more, err := g.collect(1500*time.Millisecond, g.effectDuration)
	if err != nil {
		return err
	}
	drunk = append(drunk, more...)
	if err := g.throwItem("finish drinking", ahead); err != nil {
		return err
	}
	more, err = g.collect(time.Second, g.effectDuration)
	if err != nil {
		return err
	}
	drunk = append(drunk, more...)
	g.check("potion drunk", map[string]any{"effect": drunk, "bottle": g.count("minecraft:glass_bottle"), "potion": g.count("minecraft:potion")})
	g.check("drinking stopped", !g.usingItem())
	g.setup("clear the effect")
	g.command("effect %v night_vision 0", g.s.c.name)
	if err := g.settleSetup(); err != nil {
		return err
	}

	// A snowball.
	if err := g.holdItem("snowball"); err != nil {
		return err
	}
	if err := g.throwItem("throw the snowball", ahead); err != nil {
		return err
	}
	_, err = g.actorAdded("minecraft:snowball", gameWait)
	g.check("snowball thrown", err == nil)
	if err != nil && !errors.Is(err, errTimeout) {
		return err
	}
	return g.settle(time.Second)
}
