package main

import (
	"bufio"
	"encoding/json"
	"errors"
	"fmt"
	"math"
	"os"
	"reflect"
	"strconv"
	"strings"
	"time"

	"github.com/go-gl/mathgl/mgl32"
	"github.com/google/uuid"
	"github.com/sandertv/gophertunnel/minecraft/protocol"
	"github.com/sandertv/gophertunnel/minecraft/protocol/packet"
)

// The API scenarios run on the game scenarios' machinery (game.go): a script goroutine with
// the client-side model, and console commands as "command" records the caller runs.
//
//	commands   the bot's AvailableCommands as a member, a command it may not use, then "op <name>" on the
//	           console, the AvailableCommands that follows, a chat line and commandLines as player commands
//	particles  op, then particleLines as player commands, then a snowball and an egg summoned above the bot
//	kick       "kick <name> <kickReason>" on the console; the Disconnect that follows
//	forms      "scriptevent p6:<kind>" on the console for each formAnswers kind; each form that arrives is
//	           answered (BDS shows them with the script pack in xtask/data/p6/forms-pack, Anchor's harness
//	           with its form API)
//	script     the actions of the -script file (readScript), for the example tests
//
// A player command is a CommandRequest like the client's (origin "player", a new UUID, version "latest"). The
// bot waits for the CommandOutput whose origin has that UUID, lets the server's other packets of the command
// arrive, and records one {"type":"output"} line: the command line, the CommandOutput in full (null when
// none came), whether its origin UUID is the request's, the names of the packets received meanwhile and the
// client state the command changed ("state": stone count, the first move of the player, level and
// experience, and for a time command the first world time sent). An AvailableCommands is recorded as
// {"type":"available"} with each command's name, aliases, flags, permission level and overloads (parameter
// types by name, paramType). Each answered form is a {"type":"form"} line with its JSON and the answer.

// commandLines are the commands both servers have that the commands scenario runs as an operator, in order.
var commandLines = []string{
	"say hi",
	"give @s stone 3",
	"tp ~ ~1 ~",
	"gamemode creative",
	"time set 1000",
	"effect @s speed 10 1",
	"xp 10",
	"give @s no_such_item",
	"no_such_command",
}

// memberLine is the command the commands scenario runs before it is an operator.
const memberLine = "time set 500"

// particleLines are the particle commands of the particles scenario.
var particleLines = []string{
	"particle minecraft:heart_particle ~ ~1 ~",
	"particle minecraft:villager_happy ~ ~1 ~",
	"particle minecraft:villager_angry ~ ~1 ~",
	"particle minecraft:critical_hit_emitter ~ ~1 ~",
	"particle minecraft:basic_flame_particle ~ ~1 ~",
	"particle minecraft:note_particle ~ ~1 ~",
	"particle minecraft:explosion_particle ~ ~1 ~",
	"particle minecraft:ink_emitter ~ ~1 ~",
}

// kickReason is the reason the kick scenario's console kick gives.
const kickReason = "P6 kick reason"

// formAnswers are the forms the forms scenario asks for, by kind, and the answer the bot gives each:
// "fill" fills out a custom form (fillForm), nil closes the form.
var formAnswers = []struct {
	kind   string
	answer any
}{
	{"modal", true},
	{"menu", 1},
	{"custom", "fill"},
	{"closed", nil},
}

// fillForm is an answer to a custom form: per element, null for a label, header or divider, "typed" for an
// input, true for a toggle, the maximum for a slider and 1 for a dropdown or step slider.
func fillForm(data map[string]any) []any {
	content, _ := data["content"].([]any)
	answer := make([]any, 0, len(content))
	for _, e := range content {
		element, _ := e.(map[string]any)
		switch element["type"] {
		case "input":
			answer = append(answer, "typed")
		case "toggle":
			answer = append(answer, true)
		case "slider":
			answer = append(answer, element["max"])
		case "dropdown", "step_slider":
			answer = append(answer, 1)
		default:
			answer = append(answer, nil)
		}
	}
	return answer
}

// outputWait bounds the wait for a command's CommandOutput; commandSettle is how long the bot then lets the
// command's other packets arrive.
const (
	outputWait    = 5 * time.Second
	commandSettle = time.Second
)

// playCommands is the commands scenario.
func playCommands(g *game) error {
	g.recordAvailable("spawn")
	if err := g.playerCommand(memberLine); err != nil {
		return err
	}
	if err := g.op(); err != nil {
		return err
	}
	g.step("chat", map[string]any{"message": "hello"})
	if err := g.chat("hello"); err != nil {
		return err
	}
	if err := g.settle(commandSettle); err != nil {
		return err
	}
	for _, line := range commandLines {
		if err := g.playerCommand(line); err != nil {
			return err
		}
	}
	return nil
}

// playParticles is the particles scenario.
func playParticles(g *game) error {
	if err := g.op(); err != nil {
		return err
	}
	for _, line := range particleLines {
		if err := g.playerCommand(line); err != nil {
			return err
		}
	}
	for _, entity := range []string{"snowball", "egg"} {
		g.step("summon "+entity, nil)
		at := g.offset(0, 4, 2)
		g.command("summon %v %v %v %v", entity, at[0], at[1], at[2])
		if err := g.settle(3 * time.Second); err != nil {
			return err
		}
	}
	return nil
}

// playKick is the kick scenario.
func playKick(g *game) error {
	g.s.disconnectExpected.Store(true)
	g.step("kick", nil)
	g.command("kick %v %v", g.s.c.name, kickReason)
	pk, err := g.until("Disconnect", gameWait, func(pk packet.Packet) bool {
		_, ok := pk.(*packet.Disconnect)
		return ok
	})
	if err != nil {
		return err
	}
	d := pk.(*packet.Disconnect)
	g.check("disconnect", toJSON(reflect.ValueOf(d), 0, true))
	return nil
}

// playForms is the forms scenario.
func playForms(g *game) error {
	for _, f := range formAnswers {
		g.step("form "+f.kind, nil)
		g.command("scriptevent p6:%v", f.kind)
		form, err := g.formArrived("")
		if err != nil {
			return err
		}
		answer := f.answer
		if answer == "fill" {
			answer = fillForm(form.data)
		}
		if err := g.answerForm(answer); err != nil {
			return err
		}
		if err := g.settle(500 * time.Millisecond); err != nil {
			return err
		}
	}
	return nil
}

// op makes the bot an operator with the console and waits for the AvailableCommands that follows; a server
// that sends none within outputWait is recorded as such.
func (g *game) op() error {
	g.step("op", map[string]any{"setup": true})
	g.command("op %v", g.s.c.name)
	_, err := g.until("AvailableCommands", outputWait, func(pk packet.Packet) bool {
		_, ok := pk.(*packet.AvailableCommands)
		return ok
	})
	switch {
	case errors.Is(err, errTimeout):
		g.check("op resends AvailableCommands", false)
	case err != nil:
		return err
	default:
		g.check("op resends AvailableCommands", true)
		g.recordAvailable("op")
	}
	return g.settle(commandSettle)
}

// chat sends a chat line like the client.
func (g *game) chat(message string) error {
	return g.s.send(&packet.Text{TextType: packet.TextTypeChat, SourceName: g.s.c.name, Message: message})
}

// playerCommand runs line (without "/") as the player and records its output line.
func (g *game) playerCommand(line string) error {
	g.step("command", map[string]any{"line": line})
	id := uuid.New()
	feet, attrs := g.pos, g.attrs["minecraft:player.level"]
	stone := g.count("minecraft:stone")
	err := g.s.send(&packet.CommandRequest{
		CommandLine:   "/" + line,
		CommandOrigin: protocol.CommandOrigin{Origin: protocol.CommandOriginPlayer, UUID: id},
		Version:       "latest",
	})
	if err != nil {
		return err
	}
	var names []string
	var output *packet.CommandOutput
	// The first move of the player the server sends: later corrections pull a hovering bot back down.
	var moved *mgl32.Vec3
	// The first world time the server sends (SetTime or a clock sync).
	var clock any
	seen := func(pk packet.Packet) {
		names = append(names, packetName(pk))
		switch pk := pk.(type) {
		case *packet.MovePlayer:
			if pk.EntityRuntimeID == g.runtimeID && moved == nil {
				d := pk.Position.Sub(feet)
				moved = &d
			}
		case *packet.SetTime:
			if clock == nil {
				clock = pk.Time
			}
		case *packet.SyncWorldClocks:
			if clock == nil && len(pk.SyncStates) > 0 {
				clock = pk.SyncStates[0].Time
			}
		}
	}
	_, err = g.until("CommandOutput", outputWait, func(pk packet.Packet) bool {
		seen(pk)
		o, ok := pk.(*packet.CommandOutput)
		if ok && (o.CommandOrigin.UUID == id || output == nil) {
			output = o
		}
		return ok && o.CommandOrigin.UUID == id
	})
	if err != nil && !errors.Is(err, errTimeout) {
		return err
	}
	end := time.Now().Add(commandSettle)
	for left := time.Until(end); left > 0; left = time.Until(end) {
		pk, err := g.next(left)
		if errors.Is(err, errTimeout) {
			break
		}
		if err != nil {
			return err
		}
		seen(pk)
	}
	if moved == nil {
		moved = &mgl32.Vec3{}
	}
	changed := map[string]any{
		"stone": g.count("minecraft:stone") - stone,
		"moved": []any{round2(moved[0]), round2(moved[1]), round2(moved[2])},
		"level": round2(g.attrs["minecraft:player.level"] - attrs),
		"xp":    round2(g.attrs["minecraft:player.experience"]),
	}
	// Servers sync their clocks now and then; only the step of a time command that succeeded compares
	// the time.
	if strings.HasPrefix(line, "time ") && line != memberLine && output != nil && output.SuccessCount > 0 {
		changed["clock"] = clock
	}
	g.lastOutput = output
	rec := map[string]any{"line": line, "packets": names, "state": changed, "output": nil}
	if output != nil {
		rec["output"] = toJSON(reflect.ValueOf(output), 0, true)
		rec["uuid_echoed"] = output.CommandOrigin.UUID == id
	}
	g.record("output", rec)
	return nil
}

// round2 rounds to two decimals, so float noise does not show as a difference.
func round2(f float32) float64 {
	return math.Round(float64(f)*100) / 100
}

// recordAvailable records a summary of the last AvailableCommands the bot received.
func (g *game) recordAvailable(when string) {
	if g.available == nil {
		g.record("available", map[string]any{"when": when, "commands": nil})
		return
	}
	g.record("available", map[string]any{"when": when, "commands": commandsSummary(g.available)})
}

// commandsSummary lists the commands of an AvailableCommands with their aliases and overloads.
func commandsSummary(pk *packet.AvailableCommands) []any {
	enumValues := func(index uint32) []string {
		if int(index) >= len(pk.Enums) {
			return nil
		}
		var values []string
		for _, v := range pk.Enums[index].ValueIndices {
			if int(v) < len(pk.EnumValues) {
				values = append(values, pk.EnumValues[v])
			}
		}
		return values
	}
	commands := make([]any, 0, len(pk.Commands))
	for _, c := range pk.Commands {
		var aliases []string
		if c.AliasesOffset != math.MaxUint32 {
			aliases = enumValues(c.AliasesOffset)
		}
		overloads := make([]any, 0, len(c.Overloads))
		for _, o := range c.Overloads {
			params := make([]any, 0, len(o.Parameters))
			for _, p := range o.Parameters {
				params = append(params, map[string]any{
					"name": p.Name, "type": paramType(pk, p.Type), "optional": p.Optional, "options": p.Options,
				})
			}
			overloads = append(overloads, map[string]any{"chaining": o.Chaining, "params": params})
		}
		commands = append(commands, map[string]any{
			"name": c.Name, "description": c.Description, "flags": c.Flags, "permission_level": c.PermissionLevel,
			"aliases": aliases, "overloads": overloads,
		})
	}
	return commands
}

// argTypes names the basic parameter types of AvailableCommands.
var argTypes = map[uint32]string{
	protocol.CommandArgTypeInt:             "int",
	protocol.CommandArgTypeFloat:           "float",
	protocol.CommandArgTypeRValue:          "value",
	protocol.CommandArgTypeWildcardInt:     "wildcard_int",
	protocol.CommandArgTypeOperator:        "operator",
	protocol.CommandArgTypeCompareOperator: "compare_operator",
	protocol.CommandArgTypeTarget:          "target",
	protocol.CommandArgTypeWildcardTarget:  "wildcard_target",
	protocol.CommandArgTypeFilepath:        "filepath",
	protocol.CommandArgTypeIntegerRange:    "integer_range",
	protocol.CommandArgTypeEquipmentSlots:  "equipment_slots",
	protocol.CommandArgTypeString:          "string",
	protocol.CommandArgTypeBlockPosition:   "block_position",
	protocol.CommandArgTypePosition:        "position",
	// BDS's message parameters (say, me, tell, kick) and PMMP's ARG_TYPE_MESSAGE are MessageRoot.
	protocol.CommandArgTypeMessageRoot: "message",
	protocol.CommandArgTypeRawText:     "raw_text",
	protocol.CommandArgTypeJSON:        "json",
	protocol.CommandArgTypeBlockStates: "block_states",
	protocol.CommandArgTypeCommand:     "command",
}

// paramType names a parameter type: a basic type, "enum:<name>", "softenum:<name>", "suffix:<suffix>" (a
// number with a suffix, such as xp's "L") or "type:<n>", with "+chained" for that flag and "!invalid" when
// the valid flag is missing (a suffixed parameter has none).
func paramType(pk *packet.AvailableCommands, t uint32) string {
	low := t & 0xffff
	var name string
	switch {
	case t&protocol.CommandArgSuffixed != 0:
		name = "suffix:?"
		if int(low) < len(pk.Suffixes) {
			name = "suffix:" + pk.Suffixes[low]
		}
		return name
	case t&protocol.CommandArgSoftEnum != 0:
		name = "softenum:?"
		if int(low) < len(pk.DynamicEnums) {
			name = "softenum:" + pk.DynamicEnums[low].Type
		}
	case t&protocol.CommandArgEnum != 0:
		name = "enum:?"
		if int(low) < len(pk.Enums) {
			name = "enum:" + pk.Enums[low].Type
		}
	default:
		var ok bool
		if name, ok = argTypes[low]; !ok {
			name = fmt.Sprintf("type:%v", low)
		}
	}
	if t&protocol.CommandArgTypeChainedCommand != 0 {
		name += "+chained"
	}
	if t&protocol.CommandArgValid == 0 {
		name += "!invalid"
	}
	return name
}

// formArrived waits until a form whose JSON contains text is waiting, and returns it.
func (g *game) formArrived(text string) (formRequest, error) {
	find := func() (formRequest, bool) {
		for _, f := range g.displays.forms {
			if text == "" || strings.Contains(string(f.raw), text) {
				return f, true
			}
		}
		return formRequest{}, false
	}
	var found formRequest
	err := g.waitState("a form "+strconv.Quote(text), gameWait, func() bool {
		f, ok := find()
		found = f
		return ok
	})
	return found, err
}

// answerForm answers the oldest waiting form with value (nil closes it) and records the answer.
func (g *game) answerForm(value any) error {
	resp, f, err := g.displays.answer(value)
	if err != nil {
		return err
	}
	g.record("form", map[string]any{"id": f.id, "kind": f.kind, "data": f.data, "answer": value})
	return g.s.send(resp)
}

// scriptAction is one step of a -script file.
type scriptAction struct {
	verb, arg string
	line      int
}

// readScript reads a -script file: one action per line, "verb argument", blank lines and lines starting
// with "#" left out. The verbs:
//
//	chat TEXT              send a chat line
//	command LINE           run LINE (a leading "/" is optional) as a player command and record its output
//	expect-output TEXT     the last command's output has TEXT in a message or parameter
//	expect-text TEXT       wait for a Text packet with TEXT in its message or a parameter
//	expect-title TEXT      wait for a SetTitle with TEXT
//	expect-toast TEXT      wait for a ToastRequest with TEXT in its title or message
//	expect-form TEXT       wait for a form whose JSON has TEXT
//	answer-form JSON       answer the oldest waiting form with the JSON value ("close" closes it, "fill"
//	                       fills out a custom form as fillForm does)
//	expect-score SLOT TEXT wait until the scoreboard in SLOT (sidebar, list, belowname) has TEXT in its title
//	                       or a line
//	expect-bossbar TEXT    wait until the boss bar title has TEXT
//	expect-window          wait until a container window is open
//	expect-item NAME [N]   wait until the inventory holds at least N (default 1) items named NAME
//	expect-position X Y Z  wait until the bot's feet are within half a block of (X, Y, Z)
//	click SLOT             take the stack in SLOT of the open window to the cursor (a menu click); the
//	                       response is recorded whatever its status
//	expect-disconnect TEXT wait for a Disconnect with TEXT in its message; the scenario then passes
//	wait DURATION          observe packets for DURATION (Go syntax: 500ms, 2s)
//	expect-packs N         the bot downloaded at least N resource packs when it joined
//	expect-entity TYPE     wait until an entity of the actor type TYPE (AddActor) is in view
//	interact TYPE          interact with the first entity of the actor type TYPE in view (a right click)
//	craft OUTPUT ITEM      craft OUTPUT once in the inventory's 2x2 grid from a recipe of only ITEM (one or
//	                       four cells), as the client does with the inventory open
//	place NAME X Y Z       hold NAME (from the hotbar) and place it at X Y Z on the top of the block below
//	use X Y Z              click the top of the block at X Y Z with the held item
//	break X Y Z DURATION   start breaking the block at X Y Z, wait DURATION, then destroy it; wait for air
//	expect-block X Y Z NAME [JSON]
//	                       wait until the block at X Y Z is NAME with the JSON object's states (true/false
//	                       for a byte, a number for an int, text for a string)
//	expect-drop NAME N     wait until a dropped item entity of N items named NAME is in view
//	trade TYPE            open trading with the first entity of the actor type TYPE in view, take its
//	                       first offer the inventory pays for and close the window
func readScript(path string) ([]scriptAction, error) {
	f, err := os.Open(path)
	if err != nil {
		return nil, err
	}
	defer f.Close()
	var actions []scriptAction
	sc := bufio.NewScanner(f)
	for n := 1; sc.Scan(); n++ {
		text := strings.TrimSpace(sc.Text())
		if text == "" || strings.HasPrefix(text, "#") {
			continue
		}
		verb, arg, _ := strings.Cut(text, " ")
		if !knownVerb(verb) {
			return nil, fmt.Errorf("%v:%v: unknown action %q", path, n, verb)
		}
		actions = append(actions, scriptAction{verb: verb, arg: strings.TrimSpace(arg), line: n})
	}
	return actions, sc.Err()
}

func knownVerb(verb string) bool {
	switch verb {
	case "chat", "command", "expect-output", "expect-text", "expect-title", "expect-toast", "expect-form",
		"answer-form", "expect-score", "expect-bossbar", "expect-window", "expect-item", "expect-position", "click",
		"expect-disconnect", "wait", "expect-packs", "expect-entity", "interact", "craft", "place", "use", "break",
		"expect-block", "expect-drop", "trade":
		return true
	}
	return false
}

// playScript runs the -script file's actions in order.
func playScript(g *game) error {
	actions, err := readScript(g.s.c.script)
	if err != nil {
		return err
	}
	for _, a := range actions {
		g.step(a.verb, map[string]any{"arg": a.arg, "line": a.line})
		if err := g.scriptStep(a); err != nil {
			return fmt.Errorf("script line %v (%v %v): %w", a.line, a.verb, a.arg, err)
		}
	}
	return nil
}

// scriptStep runs one action of a script.
func (g *game) scriptStep(a scriptAction) error {
	has := func(texts ...string) bool {
		for _, t := range texts {
			if strings.Contains(t, a.arg) {
				return true
			}
		}
		return false
	}
	wait := func(cond func(packet.Packet) bool) error {
		_, err := g.until(a.verb+" "+a.arg, gameWait, cond)
		return err
	}
	switch a.verb {
	case "chat":
		return g.chat(a.arg)
	case "command":
		return g.playerCommand(strings.TrimPrefix(a.arg, "/"))
	case "expect-output":
		if g.lastOutput == nil {
			return errors.New("no command output")
		}
		for _, m := range g.lastOutput.OutputMessages {
			if has(append([]string{m.Message}, m.Parameters...)...) {
				return nil
			}
		}
		return fmt.Errorf("the output %+v does not have it", g.lastOutput.OutputMessages)
	case "expect-text":
		return wait(func(pk packet.Packet) bool {
			t, ok := pk.(*packet.Text)
			return ok && has(append([]string{t.Message}, t.Parameters...)...)
		})
	case "expect-title":
		return wait(func(pk packet.Packet) bool {
			t, ok := pk.(*packet.SetTitle)
			return ok && has(t.Text)
		})
	case "expect-toast":
		return wait(func(pk packet.Packet) bool {
			t, ok := pk.(*packet.ToastRequest)
			return ok && has(t.Title, t.Message)
		})
	case "expect-form":
		_, err := g.formArrived(a.arg)
		return err
	case "answer-form":
		switch a.arg {
		case "close":
			return g.answerForm(nil)
		case "fill":
			if len(g.displays.forms) == 0 {
				return errors.New("no form to answer")
			}
			return g.answerForm(fillForm(g.displays.forms[0].data))
		}
		var value any
		if err := json.Unmarshal([]byte(a.arg), &value); err != nil {
			return err
		}
		return g.answerForm(value)
	case "expect-score":
		slot, text, _ := strings.Cut(a.arg, " ")
		return g.waitState("score", gameWait, func() bool {
			title, lines, ok := g.displays.lines(slot)
			if !ok {
				return false
			}
			for _, l := range append(lines, title) {
				if strings.Contains(l, text) {
					return true
				}
			}
			return false
		})
	case "expect-bossbar":
		return g.waitState("boss bar", gameWait, func() bool {
			return g.displays.bar != nil && has(g.displays.bar.title)
		})
	case "expect-window":
		return g.waitState("a window", gameWait, func() bool { return g.window != nil })
	case "expect-item":
		name, n, _ := strings.Cut(a.arg, " ")
		want := 1
		if n != "" {
			var err error
			if want, err = strconv.Atoi(n); err != nil {
				return err
			}
		}
		return g.waitState("items", gameWait, func() bool { return g.count(name) >= want })
	case "expect-position":
		fields := strings.Fields(a.arg)
		if len(fields) != 3 {
			return errors.New("expect-position takes X Y Z")
		}
		var feet mgl32.Vec3
		for i, f := range fields {
			v, err := strconv.ParseFloat(f, 32)
			if err != nil {
				return err
			}
			feet[i] = float32(v)
		}
		eye := feet.Add(mgl32.Vec3{0, 1.62, 0})
		return g.waitState("the position", gameWait, func() bool { return g.pos.Sub(eye).Len() < 0.5 })
	case "click":
		slot, err := strconv.ParseUint(a.arg, 10, 8)
		if err != nil {
			return err
		}
		return g.click(byte(slot))
	case "expect-disconnect":
		g.s.disconnectExpected.Store(true)
		return wait(func(pk packet.Packet) bool {
			d, ok := pk.(*packet.Disconnect)
			return ok && has(d.Message)
		})
	case "wait":
		d, err := time.ParseDuration(a.arg)
		if err != nil {
			return err
		}
		return g.settle(d)
	case "expect-packs":
		want, err := strconv.Atoi(a.arg)
		if err != nil {
			return err
		}
		if got := g.s.res.packsDownloaded; got < want {
			return fmt.Errorf("downloaded %v packs of %v offered", got, g.s.res.packsOffered)
		}
		return nil
	case "expect-entity":
		_, err := g.actorOfType(a.arg)
		return err
	case "interact":
		actor, err := g.actorOfType(a.arg)
		if err != nil {
			return err
		}
		return g.useOnEntity("interact", actor.EntityRuntimeID, actor.Position.Add(mgl32.Vec3{0, 0.25, 0}))
	case "craft":
		output, item, _ := strings.Cut(a.arg, " ")
		return g.craftInInventory(output, item)
	case "place":
		name, at, _ := strings.Cut(a.arg, " ")
		pos, _, err := blockArgs(at, 3)
		if err != nil {
			return err
		}
		slot, _, ok := g.find(name)
		if !ok || slot >= 9 {
			return fmt.Errorf("no %v in the hotbar", name)
		}
		if err := g.hold(slot); err != nil {
			return err
		}
		return g.useOnBlock("place", protocol.BlockPos{pos[0], pos[1] - 1, pos[2]}, 1, true)
	case "use":
		pos, _, err := blockArgs(a.arg, 3)
		if err != nil {
			return err
		}
		return g.useOnBlock("use", pos, 1, false)
	case "break":
		pos, rest, err := blockArgs(a.arg, 4)
		if err != nil {
			return err
		}
		d, err := time.ParseDuration(rest[0])
		if err != nil {
			return err
		}
		return g.breakBlock(pos, d)
	case "expect-block":
		pos, rest, err := blockArgs(a.arg, 4)
		if err != nil {
			return err
		}
		name, states, _ := strings.Cut(strings.Join(rest, " "), " ")
		want, err := scriptBlock(name, states)
		if err != nil {
			return err
		}
		return g.waitState("the block", gameWait, func() bool { return g.blocks[pos] == want })
	case "expect-drop":
		name, n, _ := strings.Cut(a.arg, " ")
		want, err := strconv.Atoi(n)
		if err != nil {
			return err
		}
		return g.waitState("the drop", gameWait, func() bool {
			for _, it := range g.items {
				if g.name(it.Stack) == name && int(it.Stack.Count) == want {
					return true
				}
			}
			return false
		})
	case "trade":
		return g.tradeWith(a.arg)
	}
	return fmt.Errorf("unknown action %q", a.verb)
}

// blockArgs reads a block position from the first three of at least n fields of arg and returns the
// fields after it (the last one holds the rest of arg).
func blockArgs(arg string, n int) (protocol.BlockPos, []string, error) {
	fields := strings.SplitN(arg, " ", n)
	if len(fields) < n {
		return protocol.BlockPos{}, nil, fmt.Errorf("%q: want %v arguments", arg, n)
	}
	var pos protocol.BlockPos
	for i := range pos {
		v, err := strconv.ParseInt(fields[i], 10, 32)
		if err != nil {
			return pos, nil, err
		}
		pos[i] = int32(v)
	}
	return pos, fields[3:], nil
}

// scriptBlock is the network ID of the block NAME with the states of a JSON object (empty: none).
func scriptBlock(name, states string) (uint32, error) {
	props := map[string]any{}
	if states != "" {
		var raw map[string]any
		if err := json.Unmarshal([]byte(states), &raw); err != nil {
			return 0, err
		}
		for k, v := range raw {
			switch v := v.(type) {
			case float64:
				props[k] = int32(v)
			case bool, string:
				props[k] = v
			default:
				return 0, fmt.Errorf("state %v: %T is not a block state value", k, v)
			}
		}
	}
	return networkBlockHash(name, props)
}

// actorOfType waits until an entity of the actor type is in view and returns the one with the lowest
// runtime ID.
func (g *game) actorOfType(entityType string) (*packet.AddActor, error) {
	var found *packet.AddActor
	err := g.waitState("an entity "+entityType, gameWait, func() bool {
		found = nil
		for rid, a := range g.actors {
			if a.EntityType == entityType && (found == nil || rid < found.EntityRuntimeID) {
				found = a
			}
		}
		return found != nil
	})
	return found, err
}

// craftInInventory crafts output once in the 2x2 grid of the open inventory from a recipe whose every
// ingredient is item, then closes the inventory.
func (g *game) craftInInventory(output, item string) error {
	r, err := g.recipeFor("crafting_table", output, item)
	if err != nil {
		return err
	}
	if len(r.input) != 1 && len(r.input) != 4 {
		return fmt.Errorf("the %v recipe does not fit the 2x2 grid in order", output)
	}
	var grid []byte
	for i, in := range r.input {
		if in.Count > 0 {
			grid = append(grid, byte(28+i))
		}
	}
	if err := g.openInventory(); err != nil {
		return err
	}
	if err := g.fillGrid("fill the grid", item, grid); err != nil {
		return err
	}
	if err := g.craft("craft "+output, r, grid, true); err != nil {
		return err
	}
	return g.closeWindow("close the inventory")
}

// breakBlock breaks the block at pos like a survival player: the start, d of breaking, then the
// predicted destroy; it waits until the server sends air there.
func (g *game) breakBlock(pos protocol.BlockPos, d time.Duration) error {
	g.input.look(blockCentre(pos))
	g.input.blockActions([]protocol.PlayerBlockAction{{Action: protocol.PlayerActionStartBreak, BlockPos: pos, Face: 1}}, nil)
	if err := g.settle(d); err != nil {
		return err
	}
	g.input.blockActions([]protocol.PlayerBlockAction{{Action: protocol.PlayerActionPredictDestroyBlock, BlockPos: pos, Face: 1}}, nil)
	air := state("minecraft:air", nil)
	return g.waitState("the block to break", gameWait, func() bool { return g.blocks[pos] == air })
}

// tradeWith opens trading with the first entity of the actor type in view, takes its first offer the
// inventory pays for and closes the window.
func (g *game) tradeWith(entityType string) error {
	trader, err := g.actorOfType(entityType)
	if err != nil {
		return err
	}
	g.trade = nil
	if err := g.useOnEntity("open trading", trader.EntityRuntimeID, trader.Position.Add(mgl32.Vec3{0, 1, 0})); err != nil {
		return err
	}
	if err := g.waitState("UpdateTrade", gameWait, func() bool { return g.trade != nil }); err != nil {
		return err
	}
	offers, err := decodeOffers(g.trade.SerialisedOffers)
	if err != nil {
		return fmt.Errorf("decode the offers: %w", err)
	}
	if g.window == nil {
		g.window = &packet.ContainerOpen{WindowID: g.trade.WindowID, ContainerType: g.trade.WindowType}
	}
	o, err := g.firstOffer(offers, g.trade.TradeTier)
	if err != nil {
		return err
	}
	if err := g.takeOffer(o); err != nil {
		return err
	}
	return g.closeWindow("close trading")
}

// click takes the whole stack of a slot of the open window to the cursor, as a player's click does, and
// records the response whatever its status.
func (g *game) click(slot byte) error {
	if g.window == nil {
		return errors.New("no window is open")
	}
	st := g.slots[slotRef{uint32(g.window.WindowID), slot}]
	id := g.requestID()
	res, err := g.request("click", id, protocol.ItemStack{},
		take(byte(st.item.Count), g.windowSlot(protocol.ContainerLevelEntity, slot), g.uiSlot(protocol.ContainerCursor, 0)))
	if err != nil && res.RequestID != id {
		return err
	}
	return nil
}
