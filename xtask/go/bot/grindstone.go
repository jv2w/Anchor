package main

import (
	"fmt"
	"time"

	"github.com/sandertv/gophertunnel/minecraft/protocol"
)

// The grindstone scenario (PLAN.md 10.9, user exception of 8.4) plays the grindstone like the official
// client as far as BDS shows it:
//
//	placing  a grindstone on the floor and one on the side of a block, then the supporting blocks removed
//	window   the grindstone opened, a sword put into its input and taken back, the window closed
//
// The grinding itself is not part of it: BDS refused every grindstone craft request the bot could form
// (status 9, InvalidCraftResult; NumberOfCrafts 0 or a negative Cost end the connection), so the official
// client's request shape is still unknown (xtask/data/p6/m9-facts.md, STATIONS).

// grindstoneInput is the UI slot of the grindstone's input (dragonfly).
const grindstoneInput = 16

// grindstoneStates names the network IDs of the grindstone's states.
func grindstoneStates() map[uint32]string {
	m := map[uint32]string{}
	for _, a := range []string{"standing", "hanging", "side", "multiple"} {
		for d := int32(0); d < 4; d++ {
			m[state("minecraft:grindstone", map[string]any{"attachment": a, "direction": d})] = fmt.Sprintf("%v/%v", a, d)
		}
	}
	return m
}

// blockName is a grindstone state as attachment/direction, air as "air", else the network ID.
func blockName(id uint32) string {
	if n, ok := grindstoneStates()[id]; ok {
		return n
	}
	if id == state("minecraft:air", nil) {
		return "air"
	}
	return fmt.Sprint(id)
}

func playGrindstone(g *game) error {
	if err := g.grindstonePlacing(); err != nil {
		return err
	}
	at := g.offset(0, 0, -2)
	g.setup("place the grindstone")
	g.command(`setblock %v %v %v grindstone ["attachment"="standing","direction"=0]`, at[0], at[1], at[2])
	want := state("minecraft:grindstone", map[string]any{"attachment": "standing", "direction": int32(0)})
	if err := g.waitState("the grindstone", gameWait, func() bool { return g.blocks[at] == want }); err != nil {
		return err
	}
	if err := g.settleSetup(); err != nil {
		return err
	}
	if err := g.give("iron_sword", 1); err != nil {
		return err
	}
	if err := g.openBlock("open the grindstone", at, protocol.ContainerTypeGrindstone); err != nil {
		return err
	}
	sword, _, _ := g.find("minecraft:iron_sword")
	in := g.uiSlot(protocol.ContainerGrindstoneInput, grindstoneInput)
	if _, err := g.request("sword into the grindstone", g.requestID(), protocol.ItemStack{}, place(1, g.slotInfo(sword), in)); err != nil {
		return err
	}
	out := g.uiSlot(protocol.ContainerGrindstoneInput, grindstoneInput)
	if _, err := g.request("sword out of the grindstone", g.requestID(), protocol.ItemStack{}, place(1, out, g.slotInfo(sword))); err != nil {
		return err
	}
	if err := g.closeWindow("close the grindstone"); err != nil {
		return err
	}
	return g.settle(time.Second)
}

// grindstonePlacing places a grindstone on the floor and one against a block's side, then removes the blocks
// they stand on or hang from.
func (g *game) grindstonePlacing() error {
	if err := g.give("grindstone", 2); err != nil {
		return err
	}
	slot, _, _ := g.find("minecraft:grindstone")
	if err := g.hold(slot); err != nil {
		return err
	}
	floor := g.offset(4, 0, 0)
	ground := protocol.BlockPos{floor[0], floor[1] - 1, floor[2]}
	if err := g.useOnBlock("place a grindstone on the floor", ground, 1, true); err != nil {
		return err
	}
	if err := g.waitState("the floor grindstone", gameWait, func() bool { return g.blocks[floor] != 0 && blockName(g.blocks[floor]) != "air" }); err != nil {
		return err
	}
	g.check("floor grindstone", blockName(g.blocks[floor]))
	// The placement's inventory update arrives with or after the block.
	if err := g.settle(500 * time.Millisecond); err != nil {
		return err
	}

	wall := g.offset(4, 1, 3)
	g.setup("place a wall block")
	g.command("setblock %v %v %v stone", wall[0], wall[1], wall[2])
	stone := state("minecraft:stone", nil)
	if err := g.waitState("the wall block", gameWait, func() bool { return g.blocks[wall] == stone }); err != nil {
		return err
	}
	if err := g.settleSetup(); err != nil {
		return err
	}
	side := protocol.BlockPos{wall[0] - 1, wall[1], wall[2]}
	// The west face (4) of the wall block: the grindstone goes on its west side.
	if err := g.useOnBlock("place a grindstone on a wall", wall, 4, true); err != nil {
		return err
	}
	if err := g.waitState("the wall grindstone", gameWait, func() bool { return g.blocks[side] != 0 && blockName(g.blocks[side]) != "air" }); err != nil {
		return err
	}
	g.check("wall grindstone", blockName(g.blocks[side]))
	// The placement's inventory update arrives with or after the block.
	if err := g.settle(500 * time.Millisecond); err != nil {
		return err
	}

	// Both grindstones are 3 blocks or more from the bot, so it does not pick up what they drop.
	g.step("remove the wall block", nil)
	g.command("setblock %v %v %v air", wall[0], wall[1], wall[2])
	if err := g.settle(2 * time.Second); err != nil {
		return err
	}
	g.check("wall grindstone without its wall", blockName(g.blocks[side]))

	g.step("remove the floor block", nil)
	g.command("setblock %v %v %v air", ground[0], ground[1], ground[2])
	if err := g.settle(2 * time.Second); err != nil {
		return err
	}
	g.check("floor grindstone without its floor", blockName(g.blocks[floor]))
	return nil
}
