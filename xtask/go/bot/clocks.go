package main

import "github.com/sandertv/gophertunnel/minecraft/protocol"

// The clocks scenario runs, as an operator, every form of /time the 1.26.60 command tree has (the
// world clock and its time markers): what each answers and which clock time it sends. It leaves out
// `time query gametime`: the world's age depends on how long the server ran before the bot joined.
func init() {
	gameScenarios["clocks"] = playClocks
	gameAreas["clocks"] = protocol.BlockPos{-20, -60, 12}
}

// clockLines are the /time lines of the clocks scenario, in order.
var clockLines = []string{
	"time set 1000",
	"time add 500",
	"time add -2000",
	"time set day",
	"time set minecraft:noon",
	"time set sunset next",
	"time set midnight previous",
	"time set night stay",
	"time set abc",
	"time query time",
	"time query daytime",
	"time query day",
	"time pause",
	"time query time",
	"time resume",
	"time of minecraft:overworld set 3000",
	"time of minecraft:overworld add 100",
	"time of minecraft:overworld set noon",
	"time of minecraft:overworld query time",
	"time of minecraft:overworld pause",
	"time of minecraft:overworld resume",
	"time of no_such_clock set 1",
	"time set 30000",
	"time query day",
	"time set 6000",
	"time set noon",
	"time set 6000",
	"time set noon previous",
	"time set 6000",
	"time set noon stay",
	"time set -5",
	"time set 2147483647",
	"time set 0",
	"time add -1",
	"time set Noon",
	"time of MINECRAFT:OVERWORLD query time",
	"time of overworld query time",
}

// playClocks is the clocks scenario.
func playClocks(g *game) error {
	if err := g.op(); err != nil {
		return err
	}
	for _, line := range clockLines {
		if err := g.playerCommand(line); err != nil {
			return err
		}
	}
	return nil
}
