package main

import (
	"errors"
	"fmt"
	"reflect"
	"strconv"
	"strings"
	"time"

	"github.com/go-gl/mathgl/mgl32"
	"github.com/sandertv/gophertunnel/minecraft/protocol"
	"github.com/sandertv/gophertunnel/minecraft/protocol/packet"
)

// The verification scenarios check behaviour only the official server can show; they run on the game
// scenarios' machinery. Each plays the game actions of its items and ends its steps with checks of the facts
// the comparison holds; the full packets stay in the capture for the facts file (xtask/data/p6/facts.md).
//
//	potions   every splash potion thrown at the bot's feet (the colour of the splash) and one lingering potion
//	           (the area effect cloud's entity data)
//	entities   a painting hung on a wall, a firework rocket used on the ground, an end crystal summoned and a
//	           trident thrown at a villager
//	villagers  villagers summoned until each opened trading: their variant and trade screen title (facts)
//	tradeup    trades with a villager until its tier rises: each trade's UpdateTrade and tier
//	numbers    waxed copper scraped with an axe, copper waxed with honeycomb and a wind charge thrown (facts:
//	           the numbers the server uses)
//	syntax     player commands with syntax errors and with game mode and effect names in their output
//	limits     chat lines and commands of many-byte characters near the protocol's string limits
//	others     a second player (the mover companion, -scenario mover) joins, walks, turns, attacks the bot,
//	           lands a critical hit from a jump, dies and respawns: what the bot is sent about it
//	mover      the companion of others
//	rules      game rules fallDamage and drowningDamage turned off (dodaylightcycle is off for the session)
//	rulesjoin  joins with those rules off (facts: StartGame, game rules, clocks), then turns them on again
func init() {
	for name, play := range map[string]func(*game) error{
		"potions": playPotions, "entities": playEntities,
		"villagers": playVillagers, "numbers": playNumbers, "syntax": playSyntax, "limits": playLimits,
		"limitchat": playLimitChat, "others": playOthers, "mover": playMover,
		"rules": playRules, "rulesjoin": playRulesJoin, "tradeup": playTradeUp,
	} {
		gameScenarios[name] = play
	}
	for _, name := range lineScenarios {
		gameScenarios[name] = playCommandLimit
		gameAreas[name] = protocol.BlockPos{4, -60, 12}
	}
	for name, at := range map[string]protocol.BlockPos{
		"potions": {12, -60, 4}, "entities": {12, -60, 12},
		"villagers": {12, -60, 20}, "numbers": {20, -60, -12}, "syntax": {-4, -60, 20}, "limits": {4, -60, 12},
		"limitchat": {4, -60, 12}, "others": {20, -60, 4}, "mover": {22, -60, 4},
		"rules": {4, -60, 20}, "rulesjoin": {20, -60, 12}, "tradeup": {20, -60, 20},
	} {
		gameAreas[name] = at
	}
}

// moverName is the player name of the others scenario's companion (cargo xtask p6 names players after their
// scenario).
const moverName = "P6Mover"

// criticalDelay is the time from the mover's jump to its attack: eight ticks, two past the top of the jump.
const criticalDelay = 400 * time.Millisecond

// othersName is the player name of the others scenario's bot.
const othersName = "P6Others"

// verifyLog is what the verification scenarios keep of the packets.
type verifyLog struct {
	// players are the AddPlayer packets of the other players in view, by name.
	players map[string]*packet.AddPlayer
	// respawn is the last Respawn state the server sent.
	respawn int
	// splashes are the potion splash level events since the last take.
	splashes []int32
	// hurts are the hurt events (data) of the bot itself since the last take.
	hurts []int32
	// events are the Event packets (their type and data) since the last take.
	events []any
	// tiers are the trade tiers (entity data 101) of the entities in view, by runtime ID.
	tiers map[uint64]int32
	// timeline is the locator bar packets and the other entities' death and respawn events since the
	// join, in order (consecutive repeats folded).
	timeline []string
	// sizes are the box sizes (entity data 53, 54) the other entities' SetActorData carried since the
	// join: the client keeps a dead player's small box until they come.
	sizes []string
	// healths are the health values the other entities' UpdateAttributes carried since the join.
	healths []string
}

// observeVerify keeps the packets of the verification scenarios; observe calls it for every packet.
func (g *game) observeVerify(pk packet.Packet) {
	v := &g.verify
	switch pk := pk.(type) {
	case *packet.AddPlayer:
		if v.players == nil {
			v.players = map[string]*packet.AddPlayer{}
		}
		v.players[pk.Username] = pk
	case *packet.Respawn:
		v.respawn = int(pk.State) + 1
	case *packet.LevelEvent:
		if pk.EventType == packet.LevelEventParticlesPotionSplash {
			v.splashes = append(v.splashes, pk.EventData)
		}
	case *packet.ActorEvent:
		if pk.EntityRuntimeID == g.runtimeID && pk.EventType == packet.ActorEventHurt {
			v.hurts = append(v.hurts, pk.EventData)
		}
		if pk.EntityRuntimeID != g.runtimeID && (pk.EventType == packet.ActorEventDeath || pk.EventType == packet.ActorEventSpawnAlive) {
			v.note(fmt.Sprintf("ActorEvent %v", pk.EventType))
		}
	case *packet.PlayerLocation, *packet.LocatorBar:
		v.note(packetName(pk))
	case *packet.AddActor:
		v.tier(pk.EntityRuntimeID, pk.EntityMetadata)
	case *packet.SetActorData:
		v.tier(pk.EntityRuntimeID, pk.EntityMetadata)
		w, okW := pk.EntityMetadata[protocol.EntityDataKeyWidth]
		h, okH := pk.EntityMetadata[protocol.EntityDataKeyHeight]
		if pk.EntityRuntimeID != g.runtimeID && (okW || okH) {
			v.sizes = append(v.sizes, fmt.Sprintf("%v %v", w, h))
		}
	case *packet.UpdateAttributes:
		if pk.EntityRuntimeID == g.runtimeID {
			break
		}
		for _, a := range pk.Attributes {
			if a.Name == "minecraft:health" {
				v.healths = append(v.healths, fmt.Sprint(a.Value))
			}
		}
	case *packet.Event:
		v.events = append(v.events, map[string]any{"type": fmt.Sprintf("%T", pk.Event), "event": toJSON(reflect.ValueOf(pk.Event), 0, true)})
	}
}

// note adds a line to the timeline, unless it repeats the last.
func (v *verifyLog) note(line string) {
	if n := len(v.timeline); n == 0 || v.timeline[n-1] != line {
		v.timeline = append(v.timeline, line)
	}
}

// tier keeps an entity's trade tier from its entity data.
func (v *verifyLog) tier(rid uint64, m protocol.EntityMetadata) {
	t, ok := m[protocol.EntityDataKeyTradeTier].(int32)
	if !ok {
		return
	}
	if v.tiers == nil {
		v.tiers = map[uint64]int32{}
	}
	v.tiers[rid] = t
}

// throwItem uses the held item in the air, as a throw (or the start of a draw), looking at at.
func (g *game) throwItem(name string, at mgl32.Vec3) error {
	if err := g.lookAt(at); err != nil {
		return err
	}
	st := g.slots[slotRef{protocol.WindowIDInventory, g.held}]
	data := &protocol.UseItemTransactionData{
		ActionType: protocol.UseItemActionClickAir, TriggerType: protocol.TriggerTypePlayerInput,
		HotBarSlot: int32(g.held), HeldItem: st.instance(), Position: g.pos, BlockFace: -1,
		ClientPrediction: protocol.ClientPredictionSuccess,
	}
	g.step(name, map[string]any{"transaction": toJSON(reflect.ValueOf(data), 0, true)})
	return g.s.send(&packet.InventoryTransaction{TransactionData: data})
}

// releaseItem releases the held item (a drawn trident).
func (g *game) releaseItem(name string) error {
	st := g.slots[slotRef{protocol.WindowIDInventory, g.held}]
	data := &protocol.ReleaseItemTransactionData{
		ActionType: protocol.ReleaseItemActionRelease, HotBarSlot: int32(g.held), HeldItem: st.instance(), HeadPosition: g.pos,
	}
	g.step(name, map[string]any{"transaction": toJSON(reflect.ValueOf(data), 0, true)})
	return g.s.send(&packet.InventoryTransaction{TransactionData: data})
}

// potionTypes is the number of potion types both servers have (item metadata 0 to potionTypes-1; the
// official server also has 43 to 46).
const potionTypes = 43

// playPotions is the potions scenario.
func playPotions(g *game) error {
	g.setup("resistance")
	g.command("effect %v resistance 600 255 true", g.s.c.name)
	if err := g.settleSetup(); err != nil {
		return err
	}
	feet := g.pos.Sub(mgl32.Vec3{0, 1.62, 0})
	colours := map[string]any{}
	for first := 0; first < potionTypes; first += 9 {
		last := min(first+9, potionTypes)
		g.setup("give splash potions")
		for meta := first; meta < last; meta++ {
			g.command("give %v splash_potion 1 %v", g.s.c.name, meta)
			slot := byte(meta - first)
			if err := g.waitState("the potion", gameWait, func() bool {
				return !g.slots[slotRef{protocol.WindowIDInventory, slot}].empty()
			}); err != nil {
				return err
			}
		}
		if err := g.settleSetup(); err != nil {
			return err
		}
		for meta := first; meta < last; meta++ {
			if err := g.hold(byte(meta - first)); err != nil {
				return err
			}
			g.verify.splashes = nil
			if err := g.throwItem(fmt.Sprintf("throw splash potion %v", meta), feet); err != nil {
				return err
			}
			err := g.waitState("the splash", 5*time.Second, func() bool { return len(g.verify.splashes) > 0 })
			if err != nil && !errors.Is(err, errTimeout) {
				return err
			}
			var colour any
			if len(g.verify.splashes) > 0 {
				colour = uint32(g.verify.splashes[0])
			}
			colours[fmt.Sprint(meta)] = colour
		}
	}
	g.check("splash colours", colours)

	// A lingering potion of regeneration: the cloud it leaves.
	g.setup("give a lingering potion")
	g.command("give %v lingering_potion 1 28", g.s.c.name)
	if err := g.waitState("the potion", gameWait, func() bool { return g.count("minecraft:lingering_potion") > 0 }); err != nil {
		return err
	}
	slot, _, _ := g.find("minecraft:lingering_potion")
	if err := g.hold(slot); err != nil {
		return err
	}
	if err := g.throwItem("throw the lingering potion", feet.Add(mgl32.Vec3{2, 0, 0})); err != nil {
		return err
	}
	cloud, err := g.until("the cloud", 5*time.Second, func(pk packet.Packet) bool {
		a, ok := pk.(*packet.AddActor)
		return ok && a.EntityType == "minecraft:area_effect_cloud"
	})
	if err != nil {
		return err
	}
	a := cloud.(*packet.AddActor)
	g.check("cloud", g.actorFacts(a, protocol.EntityDataKeyEffectColor,
		protocol.EntityDataKeyDataRadius, protocol.EntityDataKeyDataParticle, protocol.EntityDataKeyDataDuration,
		protocol.EntityDataKeyDataChangeOnPickup, protocol.EntityDataKeyDataPickupCount))
	g.record("fact", map[string]any{"name": "cloud data types", "value": dataTypes(a.EntityMetadata)})
	return g.settle(2 * time.Second)
}

// actorAdded waits for the AddActor of an entity type and returns it.
func (g *game) actorAdded(entityType string, d time.Duration) (*packet.AddActor, error) {
	pk, err := g.until("the "+entityType, d, func(pk packet.Packet) bool {
		a, ok := pk.(*packet.AddActor)
		return ok && a.EntityType == entityType
	})
	if err != nil {
		return nil, err
	}
	return pk.(*packet.AddActor), nil
}

// actorFacts is the comparable part of an AddActor: its type and the entity data keys of its kind (keys,
// as the entity data names them). The whole entity data is a fact (the generic keys the official server
// gives every actor differ from Anchor's: xtask/data/p6/facts.md).
func (g *game) actorFacts(a *packet.AddActor, keys ...uint32) map[string]any {
	g.record("fact", map[string]any{"name": a.EntityType + " entity data", "value": toJSON(reflect.ValueOf(a.EntityMetadata), 0, true)})
	data := map[string]any{}
	for _, k := range keys {
		data[fmt.Sprint(k)] = toJSON(reflect.ValueOf(a.EntityMetadata[k]), 0, true)
	}
	return map[string]any{"type": a.EntityType, "data": data}
}

// dataTypes is the wire type of each entity data key, by its decoded Go type.
func dataTypes(m protocol.EntityMetadata) map[string]string {
	out := make(map[string]string, len(m))
	for k, v := range m {
		out[fmt.Sprint(k)] = fmt.Sprintf("%T", v)
	}
	return out
}

// playEntities is the entities scenario.
func playEntities(g *game) error {
	wall := g.offset(2, 0, 0)
	g.setup("build a wall")
	g.command("fill %v %v %v %v %v %v stone", wall[0], wall[1], wall[2]-2, wall[0], wall[1]+3, wall[2]+2)
	stone := state("minecraft:stone", nil)
	if err := g.waitState("the wall", gameWait, func() bool { return g.blocks[wall] == stone }); err != nil {
		return err
	}
	if err := g.settleSetup(); err != nil {
		return err
	}
	for _, item := range []string{"painting", "firework_rocket", "trident"} {
		if err := g.give(item, 1); err != nil {
			return err
		}
	}

	// The painting on the wall's west face.
	slot, _, _ := g.find("minecraft:painting")
	if err := g.hold(slot); err != nil {
		return err
	}
	if err := g.useOnBlock("hang the painting", g.offset(2, 1, 0), 4, true); err != nil {
		return err
	}
	pk, err := g.until("AddPainting", gameWait, func(pk packet.Packet) bool {
		_, ok := pk.(*packet.AddPainting)
		return ok
	})
	if err != nil {
		return err
	}
	p := pk.(*packet.AddPainting)
	g.check("painting", map[string]any{"position": p.Position, "direction": p.Direction})
	g.record("fact", map[string]any{"name": "painting motive", "value": p.Title})
	if err := g.settle(time.Second); err != nil {
		return err
	}

	// The firework rocket on the ground in front of the bot.
	slot, _, _ = g.find("minecraft:firework_rocket")
	if err := g.hold(slot); err != nil {
		return err
	}
	if err := g.useOnBlock("launch the firework rocket", g.offset(-2, -1, 0), 1, true); err != nil {
		return err
	}
	rocket, err := g.actorAdded("minecraft:fireworks_rocket", gameWait)
	if err != nil {
		return err
	}
	g.check("firework rocket", g.actorFacts(rocket, protocol.EntityDataKeyOwner, protocol.EntityDataKeyDisplayFirework,
		protocol.EntityDataKeyDisplayOffset, protocol.EntityDataKeyCustomDisplay))
	g.record("fact", map[string]any{"name": "firework rocket data types", "value": dataTypes(rocket.EntityMetadata)})
	burst, err := g.until("the burst", 5*time.Second, func(pk packet.Packet) bool {
		e, ok := pk.(*packet.ActorEvent)
		return ok && e.EntityRuntimeID == rocket.EntityRuntimeID
	})
	if err != nil {
		return err
	}
	e := burst.(*packet.ActorEvent)
	g.check("firework burst", map[string]any{"event": e.EventType, "data": e.EventData})
	if err := g.settle(time.Second); err != nil {
		return err
	}

	// An end crystal.
	crystalAt := g.offset(-3, 0, 3)
	g.setup("summon an end crystal")
	g.command("summon ender_crystal %v %v %v", float32(crystalAt[0])+0.5, crystalAt[1], float32(crystalAt[2])+0.5)
	crystal, err := g.actorAdded("minecraft:ender_crystal", gameWait)
	if err != nil {
		return err
	}
	g.check("end crystal", g.actorFacts(crystal, protocol.EntityDataKeyBlockTarget))
	if err := g.settle(time.Second); err != nil {
		return err
	}

	// The trident thrown at a villager.
	targetAt := g.offset(-4, 0, -2)
	g.setup("summon the target")
	g.command("summon villager %v %v %v", float32(targetAt[0])+0.5, targetAt[1], float32(targetAt[2])+0.5)
	target, err := g.actorAdded("minecraft:villager_v2", gameWait)
	if err != nil {
		return err
	}
	g.command("effect @e[type=!player] slowness 600 255 true")
	if err := g.settle(time.Second); err != nil {
		return err
	}
	slot, _, _ = g.find("minecraft:trident")
	if err := g.hold(slot); err != nil {
		return err
	}
	if err := g.throwItem("draw the trident", target.Position.Add(mgl32.Vec3{0, 1, 0})); err != nil {
		return err
	}
	if err := g.settle(time.Second); err != nil {
		return err
	}
	if err := g.releaseItem("throw the trident"); err != nil {
		return err
	}
	trident, err := g.actorAdded("minecraft:thrown_trident", gameWait)
	if err != nil {
		return err
	}
	g.check("trident", g.actorFacts(trident))
	var flight []any
	x := trident.Position[0]
	end := time.Now().Add(4 * time.Second)
	for left := time.Until(end); left > 0; left = time.Until(end) {
		pk, err := g.next(left)
		if errors.Is(err, errTimeout) {
			break
		}
		if err != nil {
			return err
		}
		switch pk := pk.(type) {
		case *packet.MoveActorDelta:
			// The official server sends a thrown trident no SetActorMotion: its moves show its course.
			if now, ok := pk.PositionX.Value(); ok && pk.EntityRuntimeID == trident.EntityRuntimeID {
				flight = append(flight, map[string]any{"x step": sign(now - x)})
				x = now
			}
		case *packet.ActorEvent:
			if pk.EntityRuntimeID == target.EntityRuntimeID {
				flight = append(flight, map[string]any{"target event": pk.EventType, "data": pk.EventData})
			}
		case *packet.LevelSoundEvent:
			if pk.EntityType == "minecraft:thrown_trident" || strings.Contains(fmt.Sprint(pk.SoundType), "trident") {
				flight = append(flight, map[string]any{"sound": pk.SoundType})
			}
		}
	}
	g.record("fact", map[string]any{"name": "trident flight", "value": flight})
	// The trident bounces off what it hits: its course along x turns back.
	bounced, first := false, 0
	for _, f := range flight {
		step, ok := f.(map[string]any)["x step"].(int)
		if !ok || step == 0 {
			continue
		}
		if first == 0 {
			first = step
		} else if step == -first {
			bounced = true
		}
	}
	g.check("trident bounced", bounced)
	return nil
}

// sign is -1, 0 or 1 by the sign of v.
func sign(v float32) int {
	switch {
	case v > 0:
		return 1
	case v < 0:
		return -1
	}
	return 0
}

// playVillagers is the villagers scenario: villagers until six opened trading.
func playVillagers(g *game) error {
	if err := g.give("emerald", 1); err != nil {
		return err
	}
	opened := 0
	for i := 0; i < 16 && opened < 6; i++ {
		at := g.offset(2, 0, int32(i%8)-4)
		g.setup("summon a villager")
		g.command("kill @e[type=!player]")
		g.command("summon villager %v %v %v", float32(at[0])+0.5, at[1], float32(at[2])+0.5)
		v, err := g.actorAdded("minecraft:villager_v2", gameWait)
		if err != nil {
			return err
		}
		g.command("effect @e[type=!player] slowness 600 255 true")
		if err := g.settle(time.Second); err != nil {
			return err
		}
		g.trade = nil
		if err := g.useOnEntity("open trading", v.EntityRuntimeID, v.Position.Add(mgl32.Vec3{0, 1, 0})); err != nil {
			return err
		}
		err = g.waitState("UpdateTrade", 3*time.Second, func() bool { return g.trade != nil })
		value := map[string]any{"variant": v.EntityMetadata[protocol.EntityDataKeyVariant], "mark": v.EntityMetadata[protocol.EntityDataKeyMarkVariant], "title": nil}
		if err == nil {
			value["title"] = g.trade.DisplayName
			opened++
			if g.window == nil {
				g.window = &packet.ContainerOpen{WindowID: g.trade.WindowID, ContainerType: g.trade.WindowType}
			}
			if err := g.closeWindow("close trading"); err != nil {
				return err
			}
		} else if !errors.Is(err, errTimeout) {
			return err
		}
		g.record("fact", map[string]any{"name": "villager", "value": value})
	}
	g.command("kill @e[type=!player]")
	return g.settle(time.Second)
}

// playTradeUp is the tradeup scenario: a villager with an offer the bot can pay for, traded with until its
// tier rises (at most tradeUpLimit trades), each trade's UpdateTrade and tier recorded.
func playTradeUp(g *game) error {
	at := g.offset(2, 0, 0)
	if err := g.give("emerald", 64); err != nil {
		return err
	}
	for _, item := range []string{"wheat", "potato", "carrot", "beetroot"} {
		if err := g.give(item, 64); err != nil {
			return err
		}
	}
	if err := g.hold(8); err != nil {
		return err
	}
	g.step("trade until the tier rises", nil)
	g.noSteps = true
	defer func() { g.noSteps = false }()
	// The trade that raised a tier, and whether an UpdateTrade came with it; a villager whose offers run
	// out first makes way for another, at most tradeUpVillagers.
	var rise any
	for attempt := 0; attempt < tradeUpVillagers && rise == nil; attempt++ {
		rose, err := g.tradeUntilTier(at)
		if err != nil {
			return err
		}
		rise = rose
		if g.window != nil {
			if err := g.closeWindow("close trading"); err != nil {
				return err
			}
		}
		g.command("kill @e[type=!player]")
		if err := g.settle(time.Second); err != nil {
			return err
		}
	}
	g.noSteps = false
	g.check("UpdateTrade at the tier rise", rise)
	return nil
}

// tradeUntilTier summons a villager the bot can trade with and trades until its tier rises or the
// inventory pays for no offer: whether an UpdateTrade came with the trade that raised the tier, nil
// without a rise.
func (g *game) tradeUntilTier(at protocol.BlockPos) (any, error) {
	villager, o, err := g.trader(at, farmerSummon, true)
	if err != nil {
		return nil, err
	}
	// Each trade takes the first offer the inventory still pays for at the starting tier, which holds
	// until the tier rises and the loop ends.
	offers, err := decodeOffers(g.trade.SerialisedOffers)
	if err != nil {
		return nil, err
	}
	unlocked := g.trade.TradeTier
	tier := func() any { return g.verify.tiers[villager.EntityRuntimeID] }
	startTier := tier()
	var trades []any
	defer func() { g.record("fact", map[string]any{"name": "trades", "value": trades}) }()
	for i := 0; i < tradeUpLimit; i++ {
		g.trade = nil
		if err := g.takeOffer(o); err != nil {
			return nil, nil
		}
		if err := g.settle(time.Second); err != nil {
			return nil, err
		}
		resent := g.trade != nil
		trades = append(trades, map[string]any{"update_trade": resent, "tier": tier()})
		if tier() != startTier {
			return resent, nil
		}
		if resent {
			if offers, err = decodeOffers(g.trade.SerialisedOffers); err != nil {
				return nil, err
			}
		}
		if o, err = g.firstOffer(offers, unlocked); err != nil {
			return nil, nil
		}
	}
	return nil, nil
}

// tradeUpVillagers bounds the villagers the tradeup scenario tries.
const tradeUpVillagers = 3

// tradeUpLimit bounds the trades of the tradeup scenario.
const tradeUpLimit = 14

// playNumbers is the numbers scenario.
func playNumbers(g *game) error {
	blocks := []string{"waxed_copper", "waxed_exposed_copper", "waxed_weathered_copper", "waxed_oxidized_copper", "oxidized_copper", "copper_block"}
	g.setup("place copper")
	for i, b := range blocks {
		at := g.offset(2, 0, int32(i)-3)
		g.command("setblock %v %v %v %v", at[0], at[1], at[2], b)
	}
	if err := g.settleSetup(); err != nil {
		return err
	}
	for _, item := range []string{"iron_axe", "honeycomb"} {
		if err := g.give(item, 1); err != nil {
			return err
		}
	}
	axe, _, _ := g.find("minecraft:iron_axe")
	for i, b := range blocks {
		at := g.offset(2, 0, int32(i)-3)
		if b == "copper_block" {
			comb, _, _ := g.find("minecraft:honeycomb")
			if err := g.hold(comb); err != nil {
				return err
			}
		} else if err := g.hold(axe); err != nil {
			return err
		}
		g.verify.events = nil
		if err := g.useOnBlock("use on "+b, at, 5, false); err != nil {
			return err
		}
		if err := g.settle(time.Second); err != nil {
			return err
		}
		g.record("fact", map[string]any{"name": "events after " + b, "value": g.verify.events})
	}
	// A wind charge, where the server has one.
	g.setup("give a wind charge")
	g.command("give %v wind_charge 1", g.s.c.name)
	err := g.waitState("the wind charge", 3*time.Second, func() bool { return g.count("minecraft:wind_charge") > 0 })
	if errors.Is(err, errTimeout) {
		return nil
	}
	if err != nil {
		return err
	}
	charge, _, _ := g.find("minecraft:wind_charge")
	if err := g.hold(charge); err != nil {
		return err
	}
	if err := g.throwItem("throw the wind charge", g.pos.Add(mgl32.Vec3{0, -1.62, 4})); err != nil {
		return err
	}
	return g.settle(3 * time.Second)
}

// syntaxLines are the commands of the syntax scenario, run as an operator.
var syntaxLines = []string{
	"give @s no_such_item",
	"give @s no_such_item 3 and more words",
	"gamemode foo @s",
	"give @s stone abc",
	"give @s",
	"give",
	"gamemode foo",
	"tp ~ ~ abc",
	"kill @x",
	"gamemode survival",
	"gamemode adventure",
	"gamemode creative",
	"gamemode spectator",
	"gamemode survival",
}

// playSyntax is the syntax scenario.
func playSyntax(g *game) error {
	if err := g.op(); err != nil {
		return err
	}
	for _, line := range syntaxLines {
		if err := g.playerCommand(line); err != nil {
			return err
		}
	}
	return nil
}

// echoed waits for the chat echo of a message and reports whether it came.
func (g *game) echoed(message string, d time.Duration) (bool, error) {
	_, err := g.until("the echo", d, func(pk packet.Packet) bool {
		t, ok := pk.(*packet.Text)
		return ok && t.Message == message
	})
	if errors.Is(err, errTimeout) {
		return false, nil
	}
	return err == nil, err
}

// korean is n syllables of three UTF-8 bytes each.
func korean(n int) string {
	return strings.Repeat("가", n)
}

// playLimits is the limits scenario: chat lines and commands the server must take (or not) by their
// characters and bytes. The protocol's limits are Text.Message 65,536 and CommandRequest.CommandLine 1,000.
func playLimits(g *game) error {
	for _, c := range []struct{ name, message string }{
		{"ascii 255", strings.Repeat("a", 255)},
		{"ascii 512", strings.Repeat("b", 512)},
		{"ascii 513", strings.Repeat("c", 513)},
		{"korean 170", korean(170)},
		{"korean 171", korean(171)},
		{"korean 512", korean(512)},
		{"korean 600", korean(600)},
	} {
		g.step("chat "+c.name, nil)
		if err := g.chat(c.message); err != nil {
			return err
		}
		ok, err := g.echoed(c.message, 3*time.Second)
		if err != nil {
			return err
		}
		g.check("echo "+c.name, ok)
	}
	return nil
}

// lineScenarios are the command line scenarios, "line<bytes>" and "linekorean<bytes>": "/give @s " and an
// item name, the whole line of about the given bytes, ASCII or three-byte characters (fewer characters than
// bytes). Each runs as its own player, since the server may end the connection.
var lineScenarios = []string{
	"line256", "line257", "line384", "line448", "line480", "line484", "line488", "line492", "line496",
	"line500", "line504", "line505", "line506", "line507", "line508", "line511", "lineunknown600", "line512", "line1000", "linekorean387", "linekorean519",
}

// playCommandLimit runs the command line of the scenario (lineScenarios) and records whether the server
// answered it or ended the connection.
func playCommandLimit(g *game) error {
	name := g.s.c.scenario
	korean := strings.HasPrefix(name, "linekorean")
	// "lineunknown<bytes>": an unknown command of that many bytes, no item name.
	unknown := strings.HasPrefix(name, "lineunknown")
	digits := strings.TrimPrefix(strings.TrimPrefix(strings.TrimPrefix(name, "lineunknown"), "linekorean"), "line")
	bytes, err := strconv.Atoi(digits)
	if err != nil {
		return err
	}
	prefix := "give @s "
	if unknown {
		prefix = "x"
	}
	left := bytes - len("/"+prefix)
	message := strings.Repeat("e", left)
	if korean {
		message = strings.Repeat("가", left/3)
	}
	g.s.disconnectExpected.Store(true)
	err = g.playerCommand(prefix + message)
	switch {
	case err == nil:
		g.check("command line", map[string]any{"bytes": len("/" + prefix + message), "answered": g.lastOutput != nil})
	case strings.Contains(err.Error(), "connection ended"):
		g.check("command line", map[string]any{"bytes": len("/" + prefix + message), "answered": "connection ended"})
	default:
		return err
	}
	return nil
}

// rawChat is a chat Text packet written without gophertunnel's length checks, so a message over the
// protocol's limit reaches the server.
type rawChat struct{ source, message string }

func (*rawChat) ID() uint32 { return packet.IDText }

func (pk *rawChat) Marshal(io protocol.IO) {
	no := false
	io.Bool(&no)
	category, kind := uint8(packet.TextCategoryAuthoredMessage), uint8(packet.TextTypeChat)
	io.Uint8(&category)
	io.Uint8(&kind)
	io.String(&pk.source)
	io.String(&pk.message)
	empty := ""
	io.String(&empty)
	io.String(&empty)
	io.Bool(&no)
}

// playLimitChat is a chat line of 21,846 three-byte characters: 65,538 bytes, over the protocol's 65,536,
// in fewer characters. The server may end the connection.
func playLimitChat(g *game) error {
	g.s.disconnectExpected.Store(true)
	g.step("chat korean 21846", nil)
	message := korean(21846)
	if err := g.s.send(&rawChat{source: g.s.c.name, message: message}); err != nil {
		return err
	}
	ok, err := g.echoed(message, 5*time.Second)
	switch {
	case err == nil:
		g.check("echo korean 21846", ok)
	case strings.Contains(err.Error(), "connection ended"):
		g.check("echo korean 21846", "connection ended")
	default:
		return err
	}
	return nil
}

// otherFacts is what the bot was sent about another player: its AddPlayer (entity data keys and flags, game
// type, abilities, held item) and the packets about it by name.
//
// The comparable part: the entity data keys with their types, the flags, the game type, whether it holds an
// item, whether a device ID is given, the build platform and the links. The ability layers (the official server adds a
// loading screen layer while the player loads) and the values are facts (xtask/data/p6/facts.md).
func otherFacts(p *packet.AddPlayer) map[string]any {
	return map[string]any{
		"data types": dataTypes(p.EntityMetadata), "flags": breathless(p.EntityMetadata[protocol.EntityDataKeyFlags]),
		"flags2": p.EntityMetadata[protocol.EntityDataKeyFlagsTwo], "game_type": p.GameType,
		"held": p.HeldItem.Stack.NetworkID != 0, "device": p.DeviceID != "", "build_platform": p.BuildPlatform,
		"links": len(p.EntityLinks),
	}
}

// breathless is entity flags without Breathing: a player that just joined has it from its first tick
// after the spawn window, so whether an AddPlayer has it depends on timing (both servers send the
// player's flags as they are: xtask/data/p6/facts.md).
func breathless(flags any) any {
	f, ok := flags.(int64)
	if !ok {
		return flags
	}
	return f &^ (1 << protocol.EntityDataFlagBreathing)
}

// moved summarises the packets about entity rid: each move packet with its mode or flags, and the others by
// name, consecutive repeats folded.
type moveSummary struct {
	rid   uint64
	lines []string
}

func (m *moveSummary) see(pk packet.Packet) {
	var line string
	switch pk := pk.(type) {
	case *packet.MovePlayer:
		if pk.EntityRuntimeID != m.rid {
			return
		}
		line = fmt.Sprintf("MovePlayer mode %v", pk.Mode)
	case *packet.MoveActorDelta:
		if pk.EntityRuntimeID != m.rid {
			return
		}
		var has []string
		for i, o := range []protocol.Optional[float32]{pk.PositionX, pk.PositionY, pk.PositionZ, pk.RotationX, pk.RotationY, pk.RotationYHead} {
			if _, ok := o.Value(); ok {
				has = append(has, []string{"x", "y", "z", "pitch", "yaw", "head"}[i])
			}
		}
		line = fmt.Sprintf("MoveActorDelta %v ground %v", has, pk.OnGround)
	case *packet.MoveActorAbsolute:
		if pk.EntityRuntimeID != m.rid {
			return
		}
		line = fmt.Sprintf("MoveActorAbsolute flags %v", pk.Flags)
	case *packet.SetActorMotion:
		if pk.EntityRuntimeID != m.rid {
			return
		}
		line = "SetActorMotion"
	case *packet.SetActorData:
		if pk.EntityRuntimeID != m.rid {
			return
		}
		line = "SetActorData"
	case *packet.ActorEvent:
		if pk.EntityRuntimeID != m.rid {
			return
		}
		line = fmt.Sprintf("ActorEvent %v %v", pk.EventType, pk.EventData)
	case *packet.Animate:
		if pk.EntityRuntimeID != m.rid {
			return
		}
		line = fmt.Sprintf("Animate %v", pk.ActionType)
	case *packet.MobEquipment:
		if pk.EntityRuntimeID != m.rid {
			return
		}
		line = "MobEquipment"
	case *packet.MobArmourEquipment:
		if pk.EntityRuntimeID != m.rid {
			return
		}
		line = "MobArmourEquipment"
	case *packet.UpdateAttributes:
		if pk.EntityRuntimeID != m.rid {
			return
		}
		line = "UpdateAttributes"
	case *packet.PlayerLocation, *packet.LocatorBar:
		line = packetName(pk)
	default:
		return
	}
	if n := len(m.lines); n == 0 || m.lines[n-1] != line {
		m.lines = append(m.lines, line)
	}
}

// playOthers is the others scenario: the bot waits for the mover and watches it until it leaves.
func playOthers(g *game) error {
	g.step("wait for the other player", nil)
	if err := g.waitState("the other player", 60*time.Second, func() bool { return g.verify.players[moverName] != nil }); err != nil {
		return err
	}
	other := g.verify.players[moverName]
	g.check("other player", otherFacts(other))
	g.record("fact", map[string]any{"name": "other AddPlayer", "value": toJSON(reflect.ValueOf(other), 0, true)})
	g.record("fact", map[string]any{"name": "other AddPlayer data types", "value": dataTypes(other.EntityMetadata)})
	summary := &moveSummary{rid: other.EntityRuntimeID}
	g.verify.hurts = nil
	_, err := g.until("the other player to leave", 90*time.Second, func(pk packet.Packet) bool {
		summary.see(pk)
		switch pk := pk.(type) {
		case *packet.RemoveActor:
			return pk.EntityUniqueID == other.AbilityData.EntityUniqueID
		case *packet.PlayerList:
			return len(pk.Entries) > 0 && pk.Entries[0].ActionType == protocol.PlayerListActionRemove
		}
		return false
	})
	if err != nil {
		return err
	}
	g.record("fact", map[string]any{"name": "packets about the other player", "value": summary.lines})
	// The packet kinds that carry the walk, and the locator bar with the deaths and respawns in
	// order: the timing of the rest differs between servers. Teleports (MovePlayer mode 2) and the
	// on-ground flag depend on when the mover's inputs arrive, so they are left out of the check;
	// the fact above keeps them.
	moves := map[string]bool{}
	for _, l := range summary.lines {
		if strings.HasPrefix(l, "Move") && l != "MovePlayer mode 2" {
			kind, _, _ := strings.Cut(l, " flags ")
			moves[kind] = true
		}
	}
	g.check("moves of the other player", moves)
	g.check("locator bar, death and respawn", g.verify.timeline)
	g.check("box sizes of the other player", g.verify.sizes)
	g.check("health of the other player", g.verify.healths)
	g.check("hurt data", g.verify.hurts)
	return g.settle(time.Second)
}

// playMover is the companion of the others scenario.
func playMover(g *game) error {
	g.step("wait for the other player", nil)
	if err := g.waitState("the other player", 30*time.Second, func() bool { return g.verify.players[othersName] != nil }); err != nil {
		return err
	}
	other := g.verify.players[othersName]
	g.check("other player", otherFacts(other))
	if err := g.settle(3 * time.Second); err != nil {
		return err
	}
	g.step("walk", nil)
	g.input.walk(mgl32.Vec3{0, 0, 0.1}, mgl32.Vec2{}, 15)
	if err := g.settle(3 * time.Second); err != nil {
		return err
	}
	g.step("turn in place", nil)
	g.input.walk(mgl32.Vec3{}, mgl32.Vec2{0, 15}, 6)
	if err := g.settle(2 * time.Second); err != nil {
		return err
	}
	g.step("look up", nil)
	g.input.walk(mgl32.Vec3{}, mgl32.Vec2{-10, 0}, 4)
	if err := g.settle(2 * time.Second); err != nil {
		return err
	}
	g.step("attack", nil)
	if err := g.lookAt(other.Position.Add(mgl32.Vec3{0, 1, 0})); err != nil {
		return err
	}
	st := g.slots[slotRef{protocol.WindowIDInventory, g.held}]
	if err := g.s.send(&packet.InventoryTransaction{TransactionData: &protocol.UseItemOnEntityTransactionData{
		TargetEntityRuntimeID: other.EntityRuntimeID, ActionType: protocol.UseItemOnEntityActionAttack,
		HotBarSlot: int32(g.held), HeldItem: st.instance(), Position: g.pos, ClickedPosition: mgl32.Vec3{0, 1, 0},
	}}); err != nil {
		return err
	}
	if err := g.settle(2 * time.Second); err != nil {
		return err
	}
	g.step("critical attack", nil)
	g.input.jump()
	// Past the top of the jump: the bot falls.
	if err := g.settle(criticalDelay); err != nil {
		return err
	}
	if err := g.s.send(&packet.InventoryTransaction{TransactionData: &protocol.UseItemOnEntityTransactionData{
		TargetEntityRuntimeID: other.EntityRuntimeID, ActionType: protocol.UseItemOnEntityActionAttack,
		HotBarSlot: int32(g.held), HeldItem: st.instance(), Position: g.pos, ClickedPosition: mgl32.Vec3{0, 1, 0},
	}}); err != nil {
		return err
	}
	stars, err := g.until("the critical hit stars", gameWait, func(pk packet.Packet) bool {
		a, ok := pk.(*packet.Animate)
		return ok && a.EntityRuntimeID == other.EntityRuntimeID && a.ActionType == packet.AnimateActionCriticalHit
	})
	if err != nil {
		return err
	}
	g.check("critical hit stars", stars.(*packet.Animate).Data)
	if err := g.settle(2 * time.Second); err != nil {
		return err
	}
	g.step("die", nil)
	g.verify.respawn = 0
	g.command("kill %v", g.s.c.name)
	if err := g.waitState("death", gameWait, func() bool { return g.verify.respawn == packet.RespawnStateSearchingForSpawn+1 }); err != nil {
		return err
	}
	if err := g.settle(time.Second); err != nil {
		return err
	}
	g.step("respawn", nil)
	if err := g.s.send(&packet.Respawn{State: packet.RespawnStateClientReadyToSpawn, EntityRuntimeID: g.runtimeID}); err != nil {
		return err
	}
	if err := g.waitState("the respawn position", gameWait, func() bool { return g.verify.respawn == packet.RespawnStateReadyToSpawn+1 }); err != nil {
		return err
	}
	if err := g.s.send(&packet.PlayerAction{EntityRuntimeID: g.runtimeID, ActionType: protocol.PlayerActionRespawn, BlockFace: -1}); err != nil {
		return err
	}
	g.input.moved(g.pos, false)
	if err := g.settle(3 * time.Second); err != nil {
		return err
	}
	g.check("breathing after the respawn", g.breathing())
	return nil
}

// playRules is the rules scenario.
func playRules(g *game) error {
	g.setup("turn fall and drowning damage off")
	g.command("gamerule falldamage false")
	g.command("gamerule drowningdamage false")
	return g.settle(3 * time.Second)
}

// playRulesJoin is the rulesjoin scenario: the join with the rules off is the fact; the clock packets of ten
// seconds follow, then the rules are turned on again.
func playRulesJoin(g *game) error {
	g.step("watch the clock", nil)
	if err := g.settle(10 * time.Second); err != nil {
		return err
	}
	g.setup("turn fall and drowning damage on")
	g.command("gamerule falldamage true")
	g.command("gamerule drowningdamage true")
	return g.settle(2 * time.Second)
}
