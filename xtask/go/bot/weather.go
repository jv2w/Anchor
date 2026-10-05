package main

import (
	"fmt"
	"math"
	"time"

	"github.com/go-gl/mathgl/mgl32"
	"github.com/sandertv/gophertunnel/minecraft/protocol"
	"github.com/sandertv/gophertunnel/minecraft/protocol/packet"
)

// The weather API scenarios (PLAN.md 10.9 날씨와 번개) run on the game scenarios' machinery, one after another
// in one server session:
//
//	weather    each of weatherSteps on the console in its own step: every move between clear, rain and
//	           thunder, a lightning bolt 8 blocks away and one 2 blocks away; it leaves the rain on
//	joinrain   joins while it rains and records what the join said of the weather, then turns thunder on
//	joinstorm  joins during the thunder and records the same, then clears the weather
//
// Each step of the weather scenario ends with a "weather" check: the weather level events in order (type and
// data), and in a lightning step the summoned bolt's entity type and position, its sounds (type, position,
// extra data, whether they name the bolt), whether it was removed, whether fire appeared in its block, and
// the damage and burning of the bot. BDS sets the bolt's burning flag in some runs and not in others, so its
// flags are left out, and bolts elsewhere (BDS strikes at random during thunder) are not looked at. The join
// scenarios check the rain and lightning levels of StartGame and the weather level events before their first
// step.
//
// BDS raises and lowers its rain and lightning levels over about five seconds, and which level events a
// weather command sends depends on the levels reached (a command during a change sends more); Anchor's
// weather has no change over time. So every weather step and join waits rampWait, and the comparison holds
// for weather that has settled.
const (
	rampWait = 7 * time.Second
	// boltSettle is how long a lightning step waits after its bolt is removed (the fire's block updates).
	boltSettle = 500 * time.Millisecond
)

// boltType is the entity type of a lightning bolt.
const boltType = "minecraft:lightning_bolt"

// weatherLog is what the bot saw of the weather since the last check.
type weatherLog struct {
	rain, lightning float32
	events          []any
	// target is the block of the bolt the step summons, if it summons one.
	target  *protocol.BlockPos
	bolt    *packet.AddActor
	sounds  []any
	removed bool
	fire    bool
	health  float32
	burning bool
}

// observeWeather keeps the packets of the weather checks; observe calls it for every packet.
func (g *game) observeWeather(pk packet.Packet) {
	w := &g.weather
	targeted := func(p mgl32.Vec3) bool { return w.target != nil && blockOf(p) == *w.target }
	switch pk := pk.(type) {
	case *packet.StartGame:
		w.rain, w.lightning = pk.RainLevel, pk.LightningLevel
	case *packet.LevelEvent:
		if pk.EventType >= packet.LevelEventStartRaining && pk.EventType <= packet.LevelEventStopThunderstorm {
			w.events = append(w.events, map[string]any{"event": pk.EventType, "data": pk.EventData})
		}
	case *packet.AddActor:
		if pk.EntityType == boltType && targeted(pk.Position) {
			w.bolt = pk
		}
	case *packet.SetActorData:
		flags, ok := pk.EntityMetadata[protocol.EntityDataKeyFlags].(int64)
		if ok && pk.EntityRuntimeID == g.runtimeID && flags&(1<<protocol.EntityDataFlagOnFire) != 0 {
			w.burning = true
		}
	case *packet.LevelSoundEvent:
		if pk.EntityType == boltType && targeted(pk.Position) {
			unique := w.bolt != nil && pk.EntityUniqueID == w.bolt.EntityUniqueID
			w.sounds = append(w.sounds, map[string]any{
				"sound": pk.SoundType, "position": pk.Position, "extra": pk.ExtraData, "names the bolt": unique,
			})
		}
	case *packet.RemoveActor:
		if w.bolt != nil && pk.EntityUniqueID == w.bolt.EntityUniqueID {
			w.removed = true
		}
	case *packet.UpdateBlock:
		w.sawBlock(pk.Position, pk.NewBlockRuntimeID)
	case *packet.UpdateSubChunkBlocks:
		for _, e := range pk.Blocks {
			w.sawBlock(e.BlockPos, e.BlockRuntimeID)
		}
	case *packet.UpdateAttributes:
		if pk.EntityRuntimeID != g.runtimeID || w.health != 0 {
			return
		}
		for _, a := range pk.Attributes {
			if a.Name == "minecraft:health" && a.Value < a.Max {
				w.health = a.Max - a.Value
			}
		}
	}
}

// sawBlock notes fire set in the block of the bolt.
func (w *weatherLog) sawBlock(pos protocol.BlockPos, id uint32) {
	if w.target != nil && pos == *w.target && id == state("minecraft:fire", map[string]any{"age": int32(0)}) {
		w.fire = true
	}
}

// blockOf is the block a position is in.
func blockOf(p mgl32.Vec3) protocol.BlockPos {
	return protocol.BlockPos{int32(math.Floor(float64(p[0]))), int32(math.Floor(float64(p[1]))), int32(math.Floor(float64(p[2])))}
}

// checkWeather records the weather check of the step and starts a new log.
func (g *game) checkWeather() {
	w := g.weather
	value := map[string]any{"events": w.events}
	if w.target != nil {
		bolt := map[string]any{"type": nil, "position": nil}
		if w.bolt != nil {
			bolt["type"], bolt["position"] = w.bolt.EntityType, w.bolt.Position
		}
		bolt["sounds"], bolt["removed"], bolt["fire"] = w.sounds, w.removed, w.fire
		bolt["damage"], bolt["burning"] = w.health, w.burning
		value["bolt"] = bolt
	}
	g.check("weather", value)
	g.weather = weatherLog{}
}

// weatherSteps are the console commands of the weather scenario, each with its step name; a lightning step's
// %v take the position of its bolt (playWeather).
var weatherSteps = []struct{ name, line string }{
	{"rain", "weather rain"},
	{"rain again", "weather rain"},
	{"thunder", "weather thunder"},
	{"thunder again", "weather thunder"},
	{"rain after thunder", "weather rain"},
	{"clear after rain", "weather clear"},
	{"thunder after clear", "weather thunder"},
	{"clear after thunder", "weather clear"},
	{"clear again", "weather clear"},
	{"lightning far", "summon lightning_bolt %v %v %v"},
	{"lightning near", "summon lightning_bolt %v %v %v"},
	{"rain for the next player", "weather rain"},
}

// playWeather is the weather scenario.
func playWeather(g *game) error {
	g.weather = weatherLog{}
	bolts := map[string]protocol.BlockPos{"lightning far": g.offset(0, 0, -8), "lightning near": g.offset(2, 0, 0)}
	for _, s := range weatherSteps {
		line := s.line
		at, lightning := bolts[s.name]
		if lightning {
			line = fmt.Sprintf(line, at[0], at[1], at[2])
		}
		g.step(s.name, map[string]any{"line": line})
		if lightning {
			g.weather.target = &at
		}
		g.command("%v", line)
		if err := g.settleWeather(lightning); err != nil {
			return err
		}
		g.checkWeather()
	}
	return nil
}

// settleWeather lets a weather step's packets arrive: until its bolt is removed and boltSettle more in a
// lightning step, rampWait in another.
func (g *game) settleWeather(lightning bool) error {
	if !lightning {
		return g.settle(rampWait)
	}
	if err := g.waitState("the bolt's removal", gameWait, func() bool { return g.weather.removed }); err != nil {
		return err
	}
	return g.settle(boltSettle)
}

// playJoinRain is the joinrain scenario.
func playJoinRain(g *game) error {
	return g.joined("weather thunder")
}

// playJoinStorm is the joinstorm scenario.
func playJoinStorm(g *game) error {
	return g.joined("weather clear")
}

// joined records the weather the join brought and runs line on the console for the next scenario.
func (g *game) joined(line string) error {
	w := g.weather
	g.check("weather at join", map[string]any{"rain": w.rain, "lightning": w.lightning, "events": w.events})
	g.weather = weatherLog{}
	g.setup(line)
	g.command("%v", line)
	return g.settle(rampWait)
}
