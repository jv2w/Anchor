package main

import (
	"errors"
	"time"

	"github.com/sandertv/gophertunnel/minecraft/protocol"
	"github.com/sandertv/gophertunnel/minecraft/protocol/packet"
)

// The sneak scenario sneaks the way the official client does: StartSneaking with the key flags in one
// PlayerAuthInput, StopSneaking in a later one. The server answers each toggle with the bot's own
// SetActorData, whose Tick is the input's: the client applies the answer at that input's frame
// (bedrock-protocol-docs PlayerMovementOverview), so an answer that comes after the client stopped
// sneaking does not turn the sneak on again.
//
//	quick sneak  stopped on the next tick, before the first answer can arrive
//	long sneak   stopped a second later
func init() {
	gameScenarios["sneak"] = playSneak
	gameAreas["sneak"] = protocol.BlockPos{-40, -60, -4}
}

// sneakHeld is the input flags of a held sneak key.
var sneakHeld = []int{packet.InputFlagSneaking, packet.InputFlagSneakDown, packet.InputFlagSneakCurrentRaw}

// sneakAnswers presses the sets, one per input, and records for each change of the bot's own sneaking
// flag whether it carries the tick of the input that toggled it (the first and the last set).
func (g *game) sneakAnswers(name string, sets [][]int) error {
	g.step(name, nil)
	g.input.press(sets...)
	var answers []any
	sneaking := false
	end := time.Now().Add(time.Duration(len(sets))*50*time.Millisecond + 2*time.Second)
	for left := time.Until(end); left > 0; left = time.Until(end) {
		pk, err := g.next(left)
		if errors.Is(err, errTimeout) {
			break
		}
		if err != nil {
			return err
		}
		d, ok := pk.(*packet.SetActorData)
		if !ok || d.EntityRuntimeID != g.runtimeID {
			continue
		}
		flags, ok := d.EntityMetadata[protocol.EntityDataKeyFlags].(int64)
		if !ok || (flags&(1<<protocol.EntityDataFlagSneaking) != 0) == sneaking {
			continue
		}
		sneaking = !sneaking
		ticks := g.input.pressedTicks()
		toggled := ticks[0]
		if !sneaking {
			toggled = ticks[len(ticks)-1]
		}
		answers = append(answers, map[string]any{"sneaking": sneaking, "input tick": d.Tick == toggled})
	}
	g.check(name, answers)
	return nil
}

// playSneak is the sneak scenario.
func playSneak(g *game) error {
	start := append([]int{packet.InputFlagStartSneaking, packet.InputFlagSneakPressedRaw}, sneakHeld...)
	stop := []int{packet.InputFlagStopSneaking, packet.InputFlagSneakReleasedRaw}
	if err := g.sneakAnswers("quick sneak", [][]int{start, stop}); err != nil {
		return err
	}
	long := [][]int{start}
	for range 20 {
		long = append(long, sneakHeld)
	}
	return g.sneakAnswers("long sneak", append(long, stop))
}
