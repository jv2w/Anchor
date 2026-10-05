package main

import (
	"cmp"
	"context"
	"errors"
	"fmt"
	"math"
	"reflect"
	"slices"
	"sync"
	"time"

	"github.com/go-gl/mathgl/mgl32"
	"github.com/sandertv/gophertunnel/minecraft/nbt"
	"github.com/sandertv/gophertunnel/minecraft/protocol"
	"github.com/sandertv/gophertunnel/minecraft/protocol/packet"
)

// The game scenarios (craft, furnace, enchant, crops, trade) play survival gameplay the way the official
// client does it: server-authoritative inventories (ItemStackRequest), item use and entity interaction
// (InventoryTransaction) and server-authoritative block breaking (PlayerAuthInput block actions). A script
// runs in its own goroutine once the bot is initialised; it reads the packets the connection loop decodes
// (feed) in order and keeps the client-side model a client keeps: its position, inventories with stack
// network IDs, opened window, known blocks and entities. The world setup is done with server console
// commands: the bot writes {"type":"command","line":...} records and flushes, the caller (cargo xtask p6)
// runs each line on the server console, and the bot waits for the effect. Every scenario first teleports the
// bot to its own area, so the absolute positions are the same on every server with the same flat world.
// Like the client, the bot first ends its loading screen (ServerBoundLoadingScreen start and end: BDS refuses
// block interactions before the end) and then sends a PlayerAuthInput every tick (inputLoop).
//
// The script records {"type":"step"} lines (what it is about to do, with the request it sends; "setup" marks
// steps that only run console commands and "random" steps whose amounts depend on server randomness),
// {"type":"state"} lines (the items the client sees when a step ends), {"type":"response"} and
// {"type":"check"} lines (a fact it established), and the scenario passes when the script returns nil.

// gameWait bounds one wait for the server's answer.
const gameWait = 15 * time.Second

// feedLimit bounds the packets a script may be behind.
const feedLimit = 1 << 18

// gameScenarios are the game scenarios and their scripts (scenarios.go), with the API scenarios (api.go).
var gameScenarios = map[string]func(*game) error{
	"craft":      playCraft,
	"furnace":    playFurnace,
	"enchant":    playEnchant,
	"crops":      playCrops,
	"trade":      playTrade,
	"commands":   playCommands,
	"particles":  playParticles,
	"kick":       playKick,
	"forms":      playForms,
	"script":     playScript,
	"weather":    playWeather,
	"joinrain":   playJoinRain,
	"joinstorm":  playJoinStorm,
	"grindstone": playGrindstone,
}

// gameAreas are the scenarios' player positions (feet block): one area each in the spawn chunks of a flat
// world whose grass is at y -61, far enough apart that the blocks of one never touch another. The script
// scenario has none: it does not teleport.
var gameAreas = map[string]protocol.BlockPos{
	"craft":      {-12, -60, -12},
	"furnace":    {-12, -60, -4},
	"enchant":    {-12, -60, 4},
	"crops":      {-12, -60, 12},
	"trade":      {-12, -60, 20},
	"commands":   {-4, -60, -12},
	"particles":  {-4, -60, -4},
	"kick":       {-4, -60, 4},
	"forms":      {-4, -60, 12},
	"weather":    {4, -60, -12},
	"joinrain":   {4, -60, -4},
	"joinstorm":  {4, -60, 4},
	"grindstone": {4, -60, 12},
}

// isGame reports whether the scenario is a game scenario.
func isGame(scenario string) bool {
	_, ok := gameScenarios[scenario]
	return ok
}

// received is one decoded packet and its index in the capture.
type received struct {
	pk    packet.Packet
	index int
}

// feed passes the decoded packets from the connection loop to the script goroutine.
type feed struct {
	mu     sync.Mutex
	pks    []received
	closed bool
	notify chan struct{}
}

func newFeed() *feed {
	return &feed{notify: make(chan struct{}, 1)}
}

// push adds a packet; it closes the feed if the script fell too far behind.
func (f *feed) push(pk packet.Packet, index int) {
	f.mu.Lock()
	if !f.closed {
		if len(f.pks) >= feedLimit {
			f.closed = true
		} else {
			f.pks = append(f.pks, received{pk: pk, index: index})
		}
	}
	f.mu.Unlock()
	select {
	case f.notify <- struct{}{}:
	default:
	}
}

// close marks the end of the connection.
func (f *feed) close() {
	f.mu.Lock()
	f.closed = true
	f.mu.Unlock()
	select {
	case f.notify <- struct{}{}:
	default:
	}
}

// take removes and returns the waiting packets, and whether the feed is closed.
func (f *feed) take() ([]received, bool) {
	f.mu.Lock()
	defer f.mu.Unlock()
	pks := f.pks
	f.pks = nil
	return pks, f.closed
}

// stack is one inventory slot as the client sees it.
type stack struct {
	item protocol.ItemStack
	id   int32
}

// empty reports whether the slot holds nothing.
func (s stack) empty() bool {
	return s.item.Count == 0 || s.item.NetworkID == 0
}

// instance is the slot in the wire form.
func (s stack) instance() protocol.ItemInstance {
	if s.empty() {
		return protocol.ItemInstance{}
	}
	return protocol.ItemInstance{StackNetworkID: s.id, Stack: s.item}
}

// slotRef is a slot of a window: the inventory (0), the UI (124: cursor 0, crafting grid 28-40, enchanting
// 14-15, grindstone 16-17, trade ingredients 4-5, created output 50) or the opened container (its window ID).
type slotRef struct {
	window uint32
	slot   byte
}

// game is the state of one game scenario script.
type game struct {
	s      *session
	ctx    context.Context
	feed   *feed
	queue  []received
	closed bool

	names     map[int32]string
	recipes   []recipe
	runtimeID uint64
	pos       mgl32.Vec3 // eye position
	slots     map[slotRef]stack
	held      byte
	window    *packet.ContainerOpen
	blocks    map[protocol.BlockPos]uint32
	items     map[uint64]protocol.ItemInstance
	positions map[uint64]mgl32.Vec3
	unique    map[int64]uint64
	options   []protocol.EnchantmentOption
	trade     *packet.UpdateTrade
	attrs     map[string]float32
	nextReq   int32
	input     *inputLoop
	area      protocol.BlockPos
	// actors are the AddActor packets of the entities in view, by runtime ID.
	actors map[uint64]*packet.AddActor
	// random marks the steps recorded while it is set as random steps (step).
	random bool
	// flags is the bot's own entity flags (entity data key 0) as last sent.
	flags int64
	// available is the last AvailableCommands, displays the forms, scoreboards and boss bar shown, and
	// lastOutput the output of the last player command (api.go).
	available  *packet.AvailableCommands
	displays   *displays
	lastOutput *packet.CommandOutput
	// weather is what the weather scenarios saw since their last check (weather.go).
	weather weatherLog
	// verify is what the M9 verification scenarios keep (verify.go).
	verify verifyLog
	// noSteps leaves step lines out while it is set (a part whose steps differ between servers).
	noSteps bool
}

// recipe is a crafting recipe from CraftingData.
type recipe struct {
	id        string
	networkID uint32
	block     string
	input     []protocol.ItemDescriptorCount
	output    []protocol.ItemStack
}

func newGame(s *session, ctx context.Context) *game {
	return &game{
		s: s, ctx: ctx, feed: s.feed,
		names: map[int32]string{}, slots: map[slotRef]stack{},
		blocks: map[protocol.BlockPos]uint32{}, items: map[uint64]protocol.ItemInstance{},
		positions: map[uint64]mgl32.Vec3{}, unique: map[int64]uint64{}, actors: map[uint64]*packet.AddActor{},
		attrs: map[string]float32{}, nextReq: -1, displays: newDisplays(0),
	}
}

// runGame runs the scenario's script and leaves; the result goes to the session's result.
func (s *session) runGame(ctx context.Context) {
	g := newGame(s, ctx)
	err := g.run()
	if g.input != nil {
		g.input.stop()
	}
	s.gameErr = err
	if err == nil {
		s.res.ended = true
	}
	s.out.flush()
	s.stayed.Store(true)
	_ = s.conn.Close()
}

func (g *game) run() (err error) {
	defer func() {
		if r := recover(); r != nil {
			err = fmt.Errorf("script panic: %v", r)
		}
	}()
	// Catch up with everything the server sent before the bot initialised.
	if err := g.drain(); err != nil {
		return err
	}
	for _, t := range []int32{packet.LoadingScreenTypeStart, packet.LoadingScreenTypeEnd} {
		if err := g.s.send(&packet.ServerBoundLoadingScreen{Type: t}); err != nil {
			return err
		}
	}
	g.input = startInput(g.s, g.runtimeID, g.pos)
	if area, ok := gameAreas[g.s.c.scenario]; ok {
		g.area = area
		if err := g.teleport(area); err != nil {
			return err
		}
	}
	if err := gameScenarios[g.s.c.scenario](g); err != nil {
		return err
	}
	g.state()
	return nil
}

// drain observes the packets that are waiting without blocking.
func (g *game) drain() error {
	pks, closed := g.feed.take()
	for _, r := range pks {
		g.observe(r.pk)
	}
	if closed {
		return errors.New("the connection ended")
	}
	return nil
}

// next returns the next packet after observing it, waiting at most d.
func (g *game) next(d time.Duration) (packet.Packet, error) {
	deadline := time.NewTimer(d)
	defer deadline.Stop()
	for {
		if len(g.queue) > 0 {
			r := g.queue[0]
			g.queue = g.queue[1:]
			g.observe(r.pk)
			return r.pk, nil
		}
		if g.closed {
			return nil, errors.New("the connection ended")
		}
		pks, closed := g.feed.take()
		g.queue, g.closed = pks, closed
		if len(pks) > 0 || closed {
			continue
		}
		select {
		case <-g.ctx.Done():
			return nil, errors.New("timed out")
		case <-deadline.C:
			return nil, errTimeout
		case <-g.feed.notify:
		}
	}
}

// errTimeout is a wait that ended without the expected packet.
var errTimeout = errors.New("no answer in time")

// until observes packets until cond holds for one, at most d.
func (g *game) until(what string, d time.Duration, cond func(packet.Packet) bool) (packet.Packet, error) {
	end := time.Now().Add(d)
	for {
		left := time.Until(end)
		if left <= 0 {
			return nil, fmt.Errorf("waiting for %v: %w", what, errTimeout)
		}
		pk, err := g.next(left)
		if err != nil {
			return nil, fmt.Errorf("waiting for %v: %w", what, err)
		}
		if cond(pk) {
			return pk, nil
		}
	}
}

// waitState observes packets until cond holds for the model, at most d.
func (g *game) waitState(what string, d time.Duration, cond func() bool) error {
	if cond() {
		return nil
	}
	_, err := g.until(what, d, func(packet.Packet) bool { return cond() })
	return err
}

// settle observes packets for d.
func (g *game) settle(d time.Duration) error {
	end := time.Now().Add(d)
	for {
		left := time.Until(end)
		if left <= 0 {
			return nil
		}
		if _, err := g.next(left); err != nil && !errors.Is(err, errTimeout) {
			return err
		}
	}
}

// record writes one line of the given type.
func (g *game) record(kind string, fields map[string]any) {
	rec := map[string]any{"type": kind, "after": g.s.received.Load() - 1}
	for k, v := range fields {
		rec[k] = v
	}
	g.s.out.line(rec)
}

// step records what the script is about to do, after the state the previous step ended in.
func (g *game) step(name string, fields map[string]any) {
	if g.noSteps {
		return
	}
	g.state()
	if fields == nil {
		fields = map[string]any{}
	}
	fields["name"] = name
	if g.random {
		fields["random"] = true
	}
	g.record("step", fields)
}

// setup records a step that only prepares the world with console commands: the packets the server sends
// for it are not part of the comparison (cargo xtask p6 compares the commands only).
func (g *game) setup(name string) {
	g.step(name, map[string]any{"setup": true})
}

// settleSetup lets the packets that trail a setup command (inventory resends) arrive inside its step.
func (g *game) settleSetup() error {
	return g.settle(500 * time.Millisecond)
}

// state records the client's view of its items: every non-empty slot of the inventory, the UI and an
// open container, as window:slot, item name and count (stack network IDs left out).
func (g *game) state() {
	slots := map[string]any{}
	for ref, st := range g.slots {
		if st.empty() || ref.window == protocol.WindowIDOffHand || ref.window == protocol.WindowIDArmour {
			continue
		}
		window := fmt.Sprint(ref.window)
		if ref.window != protocol.WindowIDInventory && ref.window != protocol.WindowIDUI {
			window = "open"
		}
		slots[fmt.Sprintf("%v:%d", window, ref.slot)] = fmt.Sprintf("%v x%d", g.name(st.item), st.item.Count)
	}
	g.record("state", map[string]any{"slots": slots})
}

// check records a fact the script established.
func (g *game) check(name string, value any) {
	kind := "check"
	if g.noSteps {
		// A part whose steps differ between servers keeps its checks as facts.
		kind = "fact"
	}
	g.record(kind, map[string]any{"name": name, "value": value})
}

// command asks the caller to run a console command and flushes the capture so it sees the line.
func (g *game) command(format string, args ...any) {
	line := fmt.Sprintf(format, args...)
	g.record("command", map[string]any{"line": line})
	g.s.out.flush()
}

// observe updates the client-side model with one packet.
func (g *game) observe(pk packet.Packet) {
	if err := g.displays.observe(pk); err != nil {
		g.check("display error", err.Error())
	}
	g.observeWeather(pk)
	g.observeVerify(pk)
	switch pk := pk.(type) {
	case *packet.StartGame:
		g.runtimeID, g.pos = pk.EntityRuntimeID, pk.PlayerPosition
		g.displays.self = pk.EntityUniqueID
	case *packet.AvailableCommands:
		g.available = pk
	case *packet.ItemRegistry:
		for _, it := range pk.Items {
			g.names[int32(it.RuntimeID)] = it.Name
		}
	case *packet.CraftingData:
		g.craftingData(pk)
	case *packet.Respawn:
		if pk.State == packet.RespawnStateReadyToSpawn {
			g.pos = pk.Position
		}
	case *packet.MovePlayer:
		if pk.EntityRuntimeID == g.runtimeID {
			g.pos = pk.Position
			if g.input != nil {
				g.input.moved(pk.Position, pk.Mode == packet.MoveModeTeleport || pk.Mode == packet.MoveModeReset)
			}
		}
	case *packet.CorrectPlayerMovePrediction:
		g.pos = pk.Position
		if g.input != nil {
			g.input.moved(pk.Position, false)
		}
	case *packet.InventoryContent:
		for i, it := range pk.Content {
			g.setSlot(slotRef{pk.WindowID, byte(i)}, it)
		}
	case *packet.InventorySlot:
		g.setSlot(slotRef{pk.WindowID, byte(pk.Slot)}, pk.NewItem)
	case *packet.InventoryTransaction:
		// BDS tells the client about picked up items with a transaction of container actions.
		for _, a := range pk.Actions {
			w, ok := a.WindowID.Value()
			if a.SourceType == protocol.InventoryActionSourceContainer && ok && w >= 0 {
				g.setSlot(slotRef{uint32(w), byte(a.InventorySlot)}, a.NewItem)
			}
		}
	case *packet.PlayerHotBar:
		if pk.WindowID == protocol.WindowIDInventory && pk.SelectHotBarSlot {
			g.held = byte(pk.SelectedHotBarSlot)
		}
	case *packet.ContainerOpen:
		g.window = pk
	case *packet.ContainerClose:
		g.window = nil
	case *packet.UpdateBlock:
		if pk.Layer == 0 {
			g.blocks[pk.Position] = pk.NewBlockRuntimeID
		}
	case *packet.UpdateSubChunkBlocks:
		for _, e := range pk.Blocks {
			g.blocks[e.BlockPos] = e.BlockRuntimeID
		}
	case *packet.AddActor:
		g.unique[pk.EntityUniqueID] = pk.EntityRuntimeID
		g.actors[pk.EntityRuntimeID] = pk
	case *packet.AddItemActor:
		g.items[pk.EntityRuntimeID] = pk.Item
		g.unique[pk.EntityUniqueID] = pk.EntityRuntimeID
		g.positions[pk.EntityRuntimeID] = pk.Position
	case *packet.MoveActorDelta:
		if p, ok := g.positions[pk.EntityRuntimeID]; ok {
			for i, v := range []protocol.Optional[float32]{pk.PositionX, pk.PositionY, pk.PositionZ} {
				if x, ok := v.Value(); ok {
					p[i] = x
				}
			}
			g.positions[pk.EntityRuntimeID] = p
		}
	case *packet.MoveActorAbsolute:
		if _, ok := g.positions[pk.EntityRuntimeID]; ok {
			g.positions[pk.EntityRuntimeID] = pk.Position
		}
	case *packet.RemoveActor:
		if rid, ok := g.unique[pk.EntityUniqueID]; ok {
			delete(g.items, rid)
			delete(g.positions, rid)
			delete(g.actors, rid)
			delete(g.unique, pk.EntityUniqueID)
		}
	case *packet.PlayerEnchantOptions:
		g.options = pk.Options
	case *packet.UpdateTrade:
		g.trade = pk
	case *packet.SetActorData:
		if f, ok := pk.EntityMetadata[protocol.EntityDataKeyFlags].(int64); ok && pk.EntityRuntimeID == g.runtimeID {
			g.flags = f
		}
	case *packet.UpdateAttributes:
		if pk.EntityRuntimeID == g.runtimeID {
			for _, a := range pk.Attributes {
				g.attrs[a.Name] = a.Value
			}
		}
	}
}

// breathing reports the bot's own Breathing entity flag.
func (g *game) breathing() bool {
	return g.flags&(1<<protocol.EntityDataFlagBreathing) != 0
}

// craftingData keeps the shaped and shapeless recipes, in network ID order.
func (g *game) craftingData(pk *packet.CraftingData) {
	for _, r := range pk.ShapedRecipes {
		g.recipes = append(g.recipes, recipe{id: r.RecipeID, networkID: r.RecipeNetworkID, block: r.Block, input: r.Input, output: r.Output})
	}
	for _, r := range pk.ShapelessRecipes {
		g.recipes = append(g.recipes, recipe{id: r.RecipeID, networkID: r.RecipeNetworkID, block: r.Block, input: r.Input, output: r.Output})
	}
	for _, r := range pk.SmithingTransformRecipes {
		input := []protocol.ItemDescriptorCount{r.Base, r.Addition, r.Template}
		g.recipes = append(g.recipes, recipe{id: r.RecipeID, networkID: r.RecipeNetworkID, block: r.Block, input: input, output: []protocol.ItemStack{r.Result}})
	}
	slices.SortFunc(g.recipes, func(a, b recipe) int { return cmp.Compare(a.networkID, b.networkID) })
}

// setSlot stores a slot the server sent.
func (g *game) setSlot(ref slotRef, it protocol.ItemInstance) {
	if it.Stack.Count == 0 || it.Stack.NetworkID == 0 {
		delete(g.slots, ref)
		return
	}
	g.slots[ref] = stack{item: it.Stack, id: it.StackNetworkID}
}

// name is the item name of a slot's item.
func (g *game) name(it protocol.ItemStack) string {
	return g.names[it.NetworkID]
}

// itemNamed returns the network ID of an item name.
func (g *game) itemNamed(name string) (int32, error) {
	for id, n := range g.names {
		if n == name {
			return id, nil
		}
	}
	return 0, fmt.Errorf("the item registry has no %v", name)
}

// find returns the first inventory slot holding the item, by its name.
func (g *game) find(name string) (byte, stack, bool) {
	for slot := byte(0); slot < 36; slot++ {
		st, ok := g.slots[slotRef{protocol.WindowIDInventory, slot}]
		if ok && !st.empty() && g.name(st.item) == name {
			return slot, st, true
		}
	}
	return 0, stack{}, false
}

// count is the number of items of the name in the inventory.
func (g *game) count(name string) int {
	n := 0
	for slot := byte(0); slot < 36; slot++ {
		if st, ok := g.slots[slotRef{protocol.WindowIDInventory, slot}]; ok && !st.empty() && g.name(st.item) == name {
			n += int(st.item.Count)
		}
	}
	return n
}

// emptySlot returns the first empty inventory slot.
func (g *game) emptySlot() (byte, error) {
	for slot := byte(0); slot < 36; slot++ {
		if st, ok := g.slots[slotRef{protocol.WindowIDInventory, slot}]; !ok || st.empty() {
			return slot, nil
		}
	}
	return 0, errors.New("the inventory is full")
}

// inventorySnapshot is the inventory as name x count per slot, for check records.
func (g *game) inventorySnapshot() map[string]any {
	m := map[string]any{}
	for ref, st := range g.slots {
		if st.empty() {
			continue
		}
		m[fmt.Sprintf("%d:%d", ref.window, ref.slot)] = fmt.Sprintf("%v x%d #%d", g.name(st.item), st.item.Count, st.id)
	}
	return m
}

// teleport moves the bot to the feet block pos with the console and waits for the server's move.
func (g *game) teleport(pos protocol.BlockPos) error {
	g.step("teleport", map[string]any{"position": pos, "setup": true})
	return g.teleportTo(mgl32.Vec3{float32(pos[0]) + 0.5, float32(pos[1]), float32(pos[2]) + 0.5})
}

// teleportTo moves the bot's feet to the point with the console and waits for the server's move.
func (g *game) teleportTo(feet mgl32.Vec3) error {
	g.command("tp %v %v %v %v", g.s.c.name, feet[0], feet[1], feet[2])
	want := feet.Add(mgl32.Vec3{0, 1.62, 0})
	if err := g.waitState("the teleport", gameWait, func() bool { return g.pos.Sub(want).Len() < 0.01 }); err != nil {
		return err
	}
	return g.settleSetup()
}

// give runs the console give command and waits until the inventory holds at least count of the item.
func (g *game) give(name string, count int) error {
	g.setup("give " + name)
	have := g.count("minecraft:" + name)
	g.command("give %v %v %v", g.s.c.name, name, count)
	if err := g.waitState("give "+name, gameWait, func() bool { return g.count("minecraft:"+name) >= have+count }); err != nil {
		return err
	}
	return g.settleSetup()
}

// requestID returns the next client request ID: the client counts down from -1 in steps of 2.
func (g *game) requestID() int32 {
	id := g.nextReq
	g.nextReq -= 2
	return id
}

// slotInfo is the request form of an inventory slot: the hotbar (28) and the inventory (29) containers
// while no container is open, the combined container (12) while one is.
func (g *game) slotInfo(slot byte) protocol.StackRequestSlotInfo {
	st := g.slots[slotRef{protocol.WindowIDInventory, slot}]
	container := byte(protocol.ContainerCombinedHotBarAndInventory)
	if g.window == nil || g.window.WindowID == 0 {
		container = protocol.ContainerHotBar
		if slot >= 9 {
			container = protocol.ContainerInventory
		}
	}
	return protocol.StackRequestSlotInfo{
		Container: protocol.FullContainerName{ContainerID: container}, Slot: slot, StackNetworkID: st.id,
	}
}

// uiSlot is the request form of a UI slot in the container given.
func (g *game) uiSlot(container, slot byte) protocol.StackRequestSlotInfo {
	return protocol.StackRequestSlotInfo{
		Container: protocol.FullContainerName{ContainerID: container}, Slot: slot,
		StackNetworkID: g.slots[slotRef{protocol.WindowIDUI, slot}].id,
	}
}

// windowSlot is the request form of a slot of the opened container.
func (g *game) windowSlot(container, slot byte) protocol.StackRequestSlotInfo {
	var window uint32
	if g.window != nil {
		window = uint32(g.window.WindowID)
	}
	return protocol.StackRequestSlotInfo{
		Container: protocol.FullContainerName{ContainerID: container}, Slot: slot,
		StackNetworkID: g.slots[slotRef{window, slot}].id,
	}
}

// createdSlot is the request form of the created output slot during request id.
func createdSlot(id int32) protocol.StackRequestSlotInfo {
	return protocol.StackRequestSlotInfo{
		Container: protocol.FullContainerName{ContainerID: protocol.ContainerCreatedOutput}, Slot: 50, StackNetworkID: id,
	}
}

// refOf maps a request container and slot to the model's window and slot.
func (g *game) refOf(container, slot byte) (slotRef, bool) {
	switch container {
	case protocol.ContainerCombinedHotBarAndInventory, protocol.ContainerHotBar, protocol.ContainerInventory:
		return slotRef{protocol.WindowIDInventory, slot}, true
	case protocol.ContainerCursor, protocol.ContainerCraftingInput, protocol.ContainerCreatedOutput,
		protocol.ContainerEnchantingInput, protocol.ContainerEnchantingMaterial,
		protocol.ContainerTradeTwoIngredientOne, protocol.ContainerTradeTwoIngredientTwo,
		protocol.ContainerTradeTwoResultPreview, protocol.ContainerTradeIngredientOne,
		protocol.ContainerTradeIngredientTwo, protocol.ContainerTradeResultPreview,
		protocol.ContainerGrindstoneInput, protocol.ContainerGrindstoneAdditional, protocol.ContainerGrindstoneResultPreview,
		protocol.ContainerAnvilInput, protocol.ContainerAnvilMaterial, protocol.ContainerSmithingTableInput,
		protocol.ContainerSmithingTableMaterial, protocol.ContainerSmithingTableTemplate,
		protocol.ContainerBeaconPayment:
		return slotRef{protocol.WindowIDUI, slot}, true
	case protocol.ContainerOffhand:
		return slotRef{protocol.WindowIDOffHand, slot}, true
	case protocol.ContainerArmor:
		return slotRef{protocol.WindowIDArmour, slot}, true
	}
	if g.window != nil {
		return slotRef{uint32(g.window.WindowID), slot}, true
	}
	return slotRef{}, false
}

// predict applies the actions to a copy of the model the way the client predicts them; the created output
// item of a craft is result.
func (g *game) predict(actions []protocol.StackRequestAction, result protocol.ItemStack) map[slotRef]stack {
	slots := make(map[slotRef]stack, len(g.slots))
	for k, v := range g.slots {
		slots[k] = v
	}
	created := result
	move := func(count byte, src, dst protocol.StackRequestSlotInfo) {
		var item protocol.ItemStack
		if src.Container.ContainerID == protocol.ContainerCreatedOutput {
			item = created
			created.Count -= min(created.Count, uint16(count))
		} else if ref, ok := g.refOf(src.Container.ContainerID, src.Slot); ok {
			st := slots[ref]
			item = st.item
			st.item.Count -= min(st.item.Count, uint16(count))
			slots[ref] = st
		}
		if ref, ok := g.refOf(dst.Container.ContainerID, dst.Slot); ok {
			st := slots[ref]
			if st.empty() {
				st = stack{item: item}
				st.item.Count = 0
			}
			st.item.Count += uint16(count)
			slots[ref] = st
		}
	}
	for _, a := range actions {
		switch a := a.(type) {
		case *protocol.TakeStackRequestAction:
			move(a.Count, a.Source, a.Destination)
		case *protocol.PlaceStackRequestAction:
			move(a.Count, a.Source, a.Destination)
		case *protocol.ConsumeStackRequestAction:
			if ref, ok := g.refOf(a.Source.Container.ContainerID, a.Source.Slot); ok {
				st := slots[ref]
				st.item.Count -= min(st.item.Count, uint16(a.Count))
				slots[ref] = st
			}
		}
	}
	return slots
}

// request sends one ItemStackRequest with the request ID and waits for its response. On success the model
// takes the predicted items with the counts and stack network IDs of the response.
func (g *game) request(name string, id int32, result protocol.ItemStack, actions ...protocol.StackRequestAction) (protocol.ItemStackResponse, error) {
	req := protocol.ItemStackRequest{RequestID: id, Actions: actions}
	g.step(name, map[string]any{"request_id": id, "actions": toJSON(reflect.ValueOf(actions), 0, true)})
	predicted := g.predict(actions, result)
	if err := g.s.send(&packet.ItemStackRequest{Requests: []protocol.ItemStackRequest{req}}); err != nil {
		return protocol.ItemStackResponse{}, err
	}
	var res protocol.ItemStackResponse
	_, err := g.until("the response to "+name, gameWait, func(pk packet.Packet) bool {
		r, ok := pk.(*packet.ItemStackResponse)
		if !ok {
			return false
		}
		for _, x := range r.Responses {
			if x.RequestID == id {
				res = x
				return true
			}
		}
		return false
	})
	if err != nil {
		return res, err
	}
	g.record("response", map[string]any{"request_id": id, "step": name, "status": res.Status})
	if res.Status != protocol.ItemStackResponseStatusOK {
		return res, fmt.Errorf("%v: status %v", name, res.Status)
	}
	for _, info := range res.ContainerInfo {
		for _, si := range info.SlotInfo {
			ref, ok := g.refOf(info.Container.ContainerID, si.Slot)
			if !ok {
				continue
			}
			st := predicted[ref]
			if si.Count == 0 {
				delete(g.slots, ref)
				continue
			}
			st.item.Count = uint16(si.Count)
			st.id = si.StackNetworkID
			g.slots[ref] = st
		}
	}
	return res, nil
}

// place is a Place action of count items.
func place(count byte, src, dst protocol.StackRequestSlotInfo) *protocol.PlaceStackRequestAction {
	a := &protocol.PlaceStackRequestAction{}
	a.Count, a.Source, a.Destination = count, src, dst
	return a
}

// take is a Take action of count items.
func take(count byte, src, dst protocol.StackRequestSlotInfo) *protocol.TakeStackRequestAction {
	a := &protocol.TakeStackRequestAction{}
	a.Count, a.Source, a.Destination = count, src, dst
	return a
}

// consume is a Consume action of count items.
func consume(count byte, src protocol.StackRequestSlotInfo) *protocol.ConsumeStackRequestAction {
	a := &protocol.ConsumeStackRequestAction{}
	a.Count, a.Source = count, src
	return a
}

// resultItem is the deprecated craft results form of an item stack: its name instead of its network ID.
func (g *game) resultItem(it protocol.ItemStack) protocol.StackRequestItem {
	return protocol.StackRequestItem{
		Identifier: g.name(it), MetadataValue: it.MetadataValue, BlockRuntimeID: it.BlockRuntimeID,
		Count: it.Count, NBTData: it.NBTData, CanBePlacedOn: it.CanBePlacedOn, CanBreak: it.CanBreak,
	}
}

// recipeFor returns the recipe for the block with the lowest network ID whose one output is the named item
// and whose every ingredient is one of the allowed item names or tags. Recipe IDs differ between servers
// (BDS names them, Anchor numbers them like PMMP), so the bot finds recipes by what they make and take.
func (g *game) recipeFor(block, output string, allowed ...string) (recipe, error) {
	for _, r := range g.recipes {
		if r.block != block || len(r.output) != 1 || g.name(r.output[0]) != output {
			continue
		}
		ok := true
		for _, in := range r.input {
			if in.Count == 0 {
				continue
			}
			switch d := in.Descriptor.(type) {
			case *protocol.DefaultItemDescriptor:
				ok = ok && slices.Contains(allowed, d.Name)
			case *protocol.ItemTagItemDescriptor:
				ok = ok && slices.Contains(allowed, "#"+d.Tag)
			default:
				ok = false
			}
		}
		if ok {
			return r, nil
		}
	}
	return recipe{}, fmt.Errorf("CraftingData has no %v recipe making %v from %v", block, output, allowed)
}

// hold selects the hotbar slot like the client: a MobEquipment with the item in it.
func (g *game) hold(slot byte) error {
	g.held = slot
	st := g.slots[slotRef{protocol.WindowIDInventory, slot}]
	g.step("hold", map[string]any{"slot": slot, "item": g.name(st.item)})
	return g.s.send(&packet.MobEquipment{
		EntityRuntimeID: g.runtimeID, NewItem: st.instance(), InventorySlot: slot, HotBarSlot: slot,
		WindowID: protocol.WindowIDInventory,
	})
}

// useOnBlock clicks a block face with the held item (InventoryTransaction UseItem, ClickBlock). When uses
// is set the click uses one of the held item up (placing it, bone meal), and the transaction carries that
// predicted slot change, as the client's does.
func (g *game) useOnBlock(name string, pos protocol.BlockPos, face int32, uses bool) error {
	clicked := faceCentre(face)
	// Look at the clicked point first, as a player does.
	if err := g.lookAt(blockCentre(pos).Add(clicked).Sub(mgl32.Vec3{0.5, 0.5, 0.5})); err != nil {
		return err
	}
	st := g.slots[slotRef{protocol.WindowIDInventory, g.held}]
	data := &protocol.UseItemTransactionData{
		ActionType: protocol.UseItemActionClickBlock, TriggerType: protocol.TriggerTypePlayerInput,
		BlockPosition: pos, BlockFace: face, HotBarSlot: int32(g.held), HeldItem: st.instance(),
		Position: g.pos, ClickedPosition: clicked, BlockRuntimeID: g.blockAt(pos),
		ClientPrediction: protocol.ClientPredictionSuccess,
	}
	if uses && !st.empty() {
		after := st
		after.item.Count--
		data.Actions = []protocol.InventoryAction{{
			SourceType: protocol.InventoryActionSourceContainer, WindowID: protocol.Option(int8(protocol.WindowIDInventory)),
			InventorySlot: uint32(g.held), OldItem: st.instance(), NewItem: after.instance(),
		}}
	}
	g.step(name, map[string]any{"transaction": toJSON(reflect.ValueOf(data), 0, true)})
	return g.s.send(&packet.InventoryTransaction{TransactionData: data})
}

// faceCentre is the centre of a block face relative to the block's origin.
func faceCentre(face int32) mgl32.Vec3 {
	switch face {
	case 0:
		return mgl32.Vec3{0.5, 0, 0.5}
	case 2:
		return mgl32.Vec3{0.5, 0.5, 0}
	case 3:
		return mgl32.Vec3{0.5, 0.5, 1}
	case 4:
		return mgl32.Vec3{0, 0.5, 0.5}
	case 5:
		return mgl32.Vec3{1, 0.5, 0.5}
	}
	return mgl32.Vec3{0.5, 1, 0.5}
}

// lookAt turns the view towards a point and lets two inputs carry it to the server.
func (g *game) lookAt(at mgl32.Vec3) error {
	g.input.look(at)
	return g.settle(150 * time.Millisecond)
}

// useOnEntity interacts with an entity with the held item (InventoryTransaction UseItemOnEntity).
func (g *game) useOnEntity(name string, rid uint64, at mgl32.Vec3) error {
	if err := g.lookAt(at); err != nil {
		return err
	}
	st := g.slots[slotRef{protocol.WindowIDInventory, g.held}]
	data := &protocol.UseItemOnEntityTransactionData{
		TargetEntityRuntimeID: rid, ActionType: protocol.UseItemOnEntityActionInteract, HotBarSlot: int32(g.held),
		HeldItem: st.instance(), Position: g.pos, ClickedPosition: mgl32.Vec3{0, 1, 0},
	}
	g.step(name, map[string]any{"transaction": toJSON(reflect.ValueOf(data), 0, true)})
	return g.s.send(&packet.InventoryTransaction{TransactionData: data})
}

// blockAt is the runtime ID of a known block: the last update the server sent, else the flat world's.
func (g *game) blockAt(pos protocol.BlockPos) uint32 {
	if id, ok := g.blocks[pos]; ok {
		return id
	}
	name := "minecraft:air"
	switch {
	case pos[1] == -64:
		name = "minecraft:bedrock"
	case pos[1] == -63 || pos[1] == -62:
		name = "minecraft:dirt"
	case pos[1] == -61:
		name = "minecraft:grass_block"
	}
	var props map[string]any
	if name == "minecraft:bedrock" {
		props = map[string]any{"infiniburn_bit": uint8(0)}
	}
	id, _ := networkBlockHash(name, props)
	return id
}

// state is the network ID of a block state.
func state(name string, props map[string]any) uint32 {
	id, err := networkBlockHash(name, props)
	if err != nil {
		panic(err)
	}
	return id
}

// closeWindow closes the opened window like the client and waits for the server's ContainerClose.
func (g *game) closeWindow(name string) error {
	if g.window == nil {
		return errors.New("no window is open")
	}
	w := g.window
	g.step(name, map[string]any{"window": w.WindowID, "container_type": w.ContainerType})
	if err := g.s.send(&packet.ContainerClose{WindowID: w.WindowID, ContainerType: w.ContainerType}); err != nil {
		return err
	}
	_, err := g.until("ContainerClose", gameWait, func(pk packet.Packet) bool {
		_, ok := pk.(*packet.ContainerClose)
		return ok
	})
	return err
}

// openInventory opens the player's own inventory like the client (Interact OpenInventory) and waits for the
// server's ContainerOpen.
func (g *game) openInventory() error {
	g.step("open inventory", nil)
	if err := g.s.send(&packet.Interact{ActionType: packet.InteractActionOpenInventory, TargetEntityRuntimeID: g.runtimeID}); err != nil {
		return err
	}
	_, err := g.until("ContainerOpen", gameWait, func(pk packet.Packet) bool {
		_, ok := pk.(*packet.ContainerOpen)
		return ok
	})
	return err
}

// openBlock clicks a container block and waits for the server's ContainerOpen of the container type.
func (g *game) openBlock(name string, pos protocol.BlockPos, containerType byte) error {
	if err := g.useOnBlock(name, pos, 1, false); err != nil {
		return err
	}
	_, err := g.until("ContainerOpen", gameWait, func(pk packet.Packet) bool {
		o, ok := pk.(*packet.ContainerOpen)
		return ok && o.ContainerType == containerType
	})
	if err != nil {
		return err
	}
	// The server resends the inventories after opening a block; let them arrive inside this step.
	return g.settle(500 * time.Millisecond)
}

// blockCentre is the centre of a block.
func blockCentre(pos protocol.BlockPos) mgl32.Vec3 {
	return mgl32.Vec3{float32(pos[0]) + 0.5, float32(pos[1]) + 0.5, float32(pos[2]) + 0.5}
}

// offset is the area position moved by dx, dy, dz.
func (g *game) offset(dx, dy, dz int32) protocol.BlockPos {
	return protocol.BlockPos{g.area[0] + dx, g.area[1] + dy, g.area[2] + dz}
}

// decodeOffers reads the network NBT of an UpdateTrade offer list.
func decodeOffers(b []byte) (map[string]any, error) {
	var m map[string]any
	if err := nbt.UnmarshalEncoding(b, &m, nbt.NetworkLittleEndian); err != nil {
		return nil, err
	}
	return m, nil
}

// inputLoop sends a PlayerAuthInput every tick (50 ms) like the client, with the position the server last
// set, the view direction the script last chose, a teleport acknowledgement after a server move and the
// block actions the script queued.
type inputLoop struct {
	s        *session
	mu       sync.Mutex
	rid      uint64
	pos      mgl32.Vec3
	pitch    float32
	yaw      float32
	tick     uint64
	ack      bool
	actions  []protocol.PlayerBlockAction
	interact *protocol.UseItemTransactionData
	// step and steps walk the bot (verify.go): each tick moves the position by step and turns the view by
	// turn (pitch, yaw), while steps is above zero.
	step    mgl32.Vec3
	turn    mgl32.Vec2
	steps   int
	done    chan struct{}
	stopped chan struct{}
}

func startInput(s *session, rid uint64, pos mgl32.Vec3) *inputLoop {
	l := &inputLoop{s: s, rid: rid, pos: pos, done: make(chan struct{}), stopped: make(chan struct{})}
	go l.run()
	return l
}

func (l *inputLoop) run() {
	defer close(l.stopped)
	t := time.NewTicker(50 * time.Millisecond)
	defer t.Stop()
	for {
		select {
		case <-l.done:
			return
		case <-t.C:
		}
		pk, notable := l.packet()
		send := l.s.sendQuiet
		if notable {
			send = l.s.send
		}
		if err := send(pk); err != nil {
			return
		}
	}
}

// packet builds the next PlayerAuthInput and reports whether it carries more than movement (recorded).
func (l *inputLoop) packet() (*packet.PlayerAuthInput, bool) {
	l.mu.Lock()
	defer l.mu.Unlock()
	l.tick++
	flags := protocol.NewInputFlags(packet.InputFlagCount)
	flags.Set(packet.InputFlagVerticalCollision)
	var move mgl32.Vec2
	var delta mgl32.Vec3
	if l.steps > 0 {
		l.steps--
		l.pos = l.pos.Add(l.step)
		l.pitch += l.turn[0]
		l.yaw += l.turn[1]
		delta = l.step
		if l.step.Len() > 0 {
			move = mgl32.Vec2{0, 1}
			flags.Set(packet.InputFlagUp)
		}
	}
	pk := &packet.PlayerAuthInput{
		MoveVector: move, RawMoveVector: move, AnalogueMoveVector: move, Delta: delta,
		Pitch: l.pitch, Yaw: l.yaw, HeadYaw: l.yaw, Position: l.pos, InputMode: packet.InputModeMouse,
		PlayMode: packet.PlayModeNormal, InteractionModel: packet.InteractionModelCrosshair,
		InteractPitch: l.pitch, InteractYaw: l.yaw, Tick: l.tick, CameraOrientation: direction(l.pitch, l.yaw),
	}
	notable := l.ack || len(l.actions) > 0 || l.interact != nil
	if l.ack {
		flags.Set(packet.InputFlagHandledTeleport)
		l.ack = false
	}
	if len(l.actions) > 0 {
		flags.Set(packet.InputFlagPerformBlockActions)
		pk.BlockActions = protocol.Option(l.actions)
		l.actions = nil
	}
	if l.interact != nil {
		flags.Set(packet.InputFlagPerformItemInteraction)
		pk.ItemInteractionData = protocol.Option(*l.interact)
		l.interact = nil
	}
	pk.InputData = flags
	return pk, notable
}

// moved takes a position the server set; a teleport is acknowledged in the next input.
func (l *inputLoop) moved(pos mgl32.Vec3, teleport bool) {
	l.mu.Lock()
	l.pos = pos
	l.ack = l.ack || teleport
	l.mu.Unlock()
}

// look turns the view towards a point.
func (l *inputLoop) look(at mgl32.Vec3) {
	l.mu.Lock()
	defer l.mu.Unlock()
	d := at.Sub(l.pos)
	horizontal := math.Hypot(float64(d[0]), float64(d[2]))
	l.yaw = float32(math.Atan2(-float64(d[0]), float64(d[2])) * 180 / math.Pi)
	l.pitch = float32(-math.Atan2(float64(d[1]), horizontal) * 180 / math.Pi)
}

// direction is the unit view vector of a pitch and yaw in degrees.
func direction(pitch, yaw float32) mgl32.Vec3 {
	p, y := float64(pitch)*math.Pi/180, float64(yaw)*math.Pi/180
	return mgl32.Vec3{float32(-math.Sin(y) * math.Cos(p)), float32(-math.Sin(p)), float32(math.Cos(y) * math.Cos(p))}
}

// blockActions queues block actions for the next input, with an optional item interaction.
func (l *inputLoop) blockActions(actions []protocol.PlayerBlockAction, interact *protocol.UseItemTransactionData) {
	l.mu.Lock()
	l.actions = append(l.actions, actions...)
	l.interact = interact
	l.mu.Unlock()
}

// walk moves the bot by step and turns its view by turn (pitch, yaw in degrees) on each of the next ticks.
func (l *inputLoop) walk(step mgl32.Vec3, turn mgl32.Vec2, ticks int) {
	l.mu.Lock()
	l.step, l.turn, l.steps = step, turn, ticks
	l.mu.Unlock()
}

// facing is the view direction (pitch, yaw in degrees).
func (l *inputLoop) facing() (float32, float32) {
	l.mu.Lock()
	defer l.mu.Unlock()
	return l.pitch, l.yaw
}

func (l *inputLoop) stop() {
	close(l.done)
	<-l.stopped
}
