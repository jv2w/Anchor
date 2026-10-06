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

// The itemuse scenario uses items in the air the way the official client does. An item
// with a use animation (a bow, a trident, a potion to drink) starts with an InventoryTransaction UseItem
// ClickAir and, in the PlayerAuthInput of the same tick, the StartUsingItem input flag; the server answers with
// the UsingItem entity flag (bedrock-protocol-docs changelog 776: true if it agrees). A bow or trident ends with
// an InventoryTransaction ReleaseItem; a drink ends with a second ClickAir once its time has passed. A thrown
// item is a ClickAir alone.
//
//	bow      drawn for one second and released: the UsingItem flag while drawing, the arrow
//	trident  drawn for one second and released: the flag and the throw sound
//	drink    a night vision potion drunk: the flag, the effect and the bottle left
//	snowball thrown: the snowball
//	splash   night vision splash potions broken at the feet and on walls 1.5 to 3.5 blocks away (one a long
//	         potion, one above the head): the effect durations
func init() {
	gameScenarios["itemuse"] = playItemUse
	gameAreas["itemuse"] = protocol.BlockPos{-20, -60, -4}
}

// nightVision is the item metadata of the night vision potion (3 minutes) on both servers.
const nightVision = 5

// longNightVision is the long night vision potion (8 minutes).
const longNightVision = 6

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

	// The trident and the sound of its throw.
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
	feetY := g.pos[1] - 1.62
	sounds, err := g.collect(time.Second, func(pk packet.Packet) any {
		if s, ok := pk.(*packet.LevelSoundEvent); ok {
			return s
		}
		return nil
	})
	if err != nil {
		return err
	}
	var all []any
	throw := map[string]any{}
	for _, v := range sounds {
		s := v.(*packet.LevelSoundEvent)
		all = append(all, map[string]any{"sound": s.SoundType, "entity": s.EntityType, "position": s.Position})
		if s.SoundType == "item.trident.throw" {
			throw = map[string]any{"data": s.ExtraData, "height above the feet": fmt.Sprintf("%.2f", s.Position[1]-feetY)}
		}
	}
	// The entity type of a sound without an entity ("" on BDS) differs between the servers: a fact.
	g.record("fact", map[string]any{"name": "trident sounds", "value": all})
	g.check("trident throw sound", throw)
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
	if err := g.settle(time.Second); err != nil {
		return err
	}

	// Splash potions at the feet and on a wall 1.5, 2.5 and 3.5 blocks away (where they break does not depend
	// on how each server moves them), one of them a long potion and one above the head: the effect durations.
	feet := g.pos.Sub(mgl32.Vec3{0, 1.62, 0})
	durations := map[string]any{}
	for _, s := range []struct {
		wall   int32
		height float32
		meta   int
	}{{0, 0, nightVision}, {2, 1, nightVision}, {3, 1, nightVision}, {4, 1.6, nightVision}, {3, 1, longNightVision}, {2, 2.4, nightVision}} {
		target := feet
		x := g.area[0] + s.wall
		if s.wall > 0 {
			g.setup("build a wall")
			g.command("fill %v %v %v %v %v %v stone", x, g.area[1], g.area[2]-1, x, g.area[1]+3, g.area[2]+1)
			stone := state("minecraft:stone", nil)
			top := protocol.BlockPos{x, g.area[1] + 3, g.area[2] + 1}
			if err := g.waitState("the wall", gameWait, func() bool { return g.blocks[top] == stone }); err != nil {
				return err
			}
			if err := g.settleSetup(); err != nil {
				return err
			}
			target = mgl32.Vec3{float32(x), feet[1] + s.height, feet[2]}
		}
		if err := g.giveMeta("splash_potion", s.meta); err != nil {
			return err
		}
		if err := g.holdItem("splash_potion"); err != nil {
			return err
		}
		name := fmt.Sprintf("potion %v, %.1f away, %.1f up", s.meta, target[0]-feet[0], s.height)
		if err := g.throwItem("splash "+name, target); err != nil {
			return err
		}
		var at any
		got, err := g.collect(3*time.Second, func(pk packet.Packet) any {
			if e, ok := pk.(*packet.LevelEvent); ok && e.EventType == packet.LevelEventParticlesPotionSplash {
				at = e.Position
			}
			return g.effectDuration(pk)
		})
		if err != nil {
			return err
		}
		if s.height > 1.8 {
			// Above the head the servers differ by a few ticks (xtask/data/p6/facts.md): a fact.
			g.record("fact", map[string]any{"name": "splash duration " + name, "value": got})
		} else {
			durations[name] = got
		}
		g.record("fact", map[string]any{"name": "splash " + name, "value": map[string]any{"splash": at, "feet": feet}})
		g.setup("clear the effect and the wall")
		g.command("effect %v night_vision 0", g.s.c.name)
		if s.wall > 0 {
			g.command("fill %v %v %v %v %v %v air", x, g.area[1], g.area[2]-1, x, g.area[1]+3, g.area[2]+1)
		}
		if err := g.settleSetup(); err != nil {
			return err
		}
	}
	g.check("splash durations", durations)
	return nil
}
