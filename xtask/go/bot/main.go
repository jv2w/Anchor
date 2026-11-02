// Command bot is a Bedrock client built from gophertunnel's packet, login and protocol packages, go-raknet and
// go-nethernet. It joins a server, records every packet it receives and checks one scenario. It mirrors the
// client side of gophertunnel's minecraft/conn.go but does not use minecraft.Dialer, so the login request can
// carry a token from a test issuer.
//
//	bot -addr HOST:PORT -scenario SCENARIO -out FILE.jsonl [-transport raknet|nethernet|auto]
//	    [-signaling-addr HOST:PORT] [-token-key key.pem -kid K -iss ISS] [-name N] [-timeout 30s] [-stay 2s] [-login-size BYTES]
//	    [-segment-size BYTES] [-first-send-delay D] [-script FILE]
//	bot issuer -dir DIR -kid K -iss ISS
//	bot worlddata -world CAPTURE.world.jsonl -data DIR   (worlddata.go)
//
// Transports (-addr is the RakNet address; -signaling-addr is the NetherNet signaling address, -addr when not
// given: a server started on port 0 binds its signaling TCP port apart from its RakNet UDP port):
//
//	raknet     go-raknet: 0xfe batch header, encryption after ServerToClientHandshake (the default)
//	nethernet  GET /v1/join over https, then http; dial the first that answered with go-nethernet, whose
//	           offer carries an identity: the online token (domain -iss), else a self-signed ES384 token
//	           whose cpk is the bot key (domain "self"). No batch header, no encryption.
//	auto       nethernet within 10 s, on any failure RakNet on the same address
//
// Over NetherNet the bot reads and writes the data channels itself (segments.go): it records every
// segment the server sends and writes its own messages in segments of -segment-size payload bytes
// (default 262,143, go-nethernet's). -first-send-delay waits before the first packet, so the result
// shows whether the server sends anything unprompted.
//
// Scenarios:
//
//	spawn          offline login, reach PlayStatus(PlayerSpawn), send SetLocalPlayerAsInitialised, stay -stay
//	big-skin       spawn with a valid ClientData whose Login request is about -login-size bytes
//	online         spawn with a login.Encode request whose token is minted with -token-key (RS256)
//	kicked         spawn, then wait up to -stay for a Disconnect; pass only if its reason is
//	               DisconnectReasonLoggedInOtherLocation
//	rejected       pass if a Disconnect arrives before spawning
//	stall          log in and never answer ResourcePacksInfo (the output is flushed when it arrives); pass if
//	               the server ends the session with a Disconnect or a failed PlayStatus ("play_status")
//	blocks         spawn, then stay until a block update sets minecraft:diamond_block anywhere (the end
//	               marker; at most -stay); the caller changes blocks meanwhile (cargo xtask p6)
//	bind-mismatch  nethernet only: the offer identity proves a second key, the Login the bot key; pass if
//	               the server sends a Disconnect or closes the connection before spawning
//	idle, prelogin-limit  RakNet server limit checks (limits.go)
//	dive, trade    survival gameplay like the official client (game.go, scenarios.go);
//	               the caller runs the console commands the bot writes as "command" records
//	commands, kick, forms  the API scenarios (api.go): player commands and their output, a console kick
//	               and forms, on the game scenarios' machinery
//	script         the actions of the -script file (api.go: chat, commands, expected texts, items, positions,
//	               forms, scores, boss bar, titles, menu clicks), for the example tests
//	weather, joinrain, joinstorm  weather changes and lightning, and joining in rain and thunder (weather.go)
//	boltpeaceful, bolteasy, bolthard  lightning damage at each difficulty (weather.go)
//	sneak          quick and long sneaks and the server's answers (sneak.go)
//	potions, entities, villagers, numbers, syntax, limits, limitchat, others, mover, rules,
//	rulesjoin, tradeup, line<bytes>, linekorean<bytes>  the verification scenarios (verify.go); the line
//	               scenarios are the byte counts of lineScenarios
//
// -scenario's usage text lists every scenario name (scenarioList).
//
// The output holds one JSON object per line, each with a "type": "join" (one /v1/join attempt), "sdp" (the
// offer or answer SDP and its line keys), "identity" (the answer's decoded a=identity), "segment" (one NetherNet
// segment the server sent: channel label, remaining segments, payload length; the first 200 and every
// segment of a segmented message), "raknet" (RakNet
// handshake probe), "pong", "batch" (raw batch facts), "packet" (a received packet), "sent" (a packet the bot
// sent, with its fields for a SubChunkRequest and the probe name for the spawn scenario's extra requests; the
// output is flushed after SetLocalPlayerAsInitialised), "subchunk" (after each SubChunk packet: per entry the
// offset, the result and the payload checked against Anchor's strict sub-chunk rules, chunk.go), "blocks" (the
// block network ID check) and a final "result". ItemRegistry, CreativeContent and CraftingData are also written
// in full, one {"index","name","fields"} line each, to the -out path with ".jsonl" (if any) replaced by
// ".items.jsonl", and LevelChunk, SubChunk, BiomeDefinitionList, JigsawStructureData and CurrentStructureFeature
// to ".world.jsonl": nothing is shortened and NBT values keep their tag types ({"byte":1}). In the game
// scenarios every other received packet line keeps its fields in this full form, the "sent" lines of the
// inventory and interaction packets carry their full fields, and the script adds "step", "response",
// "check" and "command" lines (game.go), and "output", "available" and "form" lines (api.go).
//
// In the sub-chunk request mode the bot asks for the sub-chunks of the 3x3 chunks around the spawn chunk, from
// index -4 up to SubChunkLimit sub-chunks, as those chunks arrive. After spawning, the spawn scenario sends three
// more SubChunkRequests (chunk.go): every index -4 to 19 of the spawn chunk, the indexes -5 and 20, and a chunk
// 64 chunks away that the server never sent.
//
// The process exits 0 only if the scenario passed, 1 if it failed and 2 on bad arguments or I/O errors.
package main

import (
	"bufio"
	"context"
	"encoding/json"
	"errors"
	"flag"
	"fmt"
	"maps"
	"os"
	"slices"
	"strings"
	"sync"
	"time"
)

// config holds the command line flags.
type config struct {
	addr, scenario, out string
	signalingAddr       string
	transport           string
	tokenKey, kid, iss  string
	name                string
	timeout, stay       time.Duration
	loginSize           int
	segmentSize         int
	firstSendDelay      time.Duration
	script              string
}

func parseFlags(args []string) (config, error) {
	var c config
	fs := flag.NewFlagSet("bot", flag.ContinueOnError)
	fs.StringVar(&c.addr, "addr", "", "server address HOST:PORT")
	fs.StringVar(&c.signalingAddr, "signaling-addr", "", "NetherNet signaling address HOST:PORT (default -addr)")
	fs.StringVar(&c.scenario, "scenario", "spawn", scenarioList())
	fs.StringVar(&c.out, "out", "", "output JSON lines file")
	fs.StringVar(&c.transport, "transport", "raknet", "raknet, nethernet or auto")
	fs.StringVar(&c.tokenKey, "token-key", "", "online: PEM RSA private key of the test issuer")
	fs.StringVar(&c.kid, "kid", "", "online: key ID put in the token header")
	fs.StringVar(&c.iss, "iss", "", "online: token issuer")
	fs.StringVar(&c.name, "name", "AnchorBot", "player name")
	fs.DurationVar(&c.timeout, "timeout", 30*time.Second, "time limit for the whole scenario")
	fs.DurationVar(&c.stay, "stay", 2*time.Second, "time to stay connected after spawning (kicked: the longest wait)")
	fs.IntVar(&c.loginSize, "login-size", 6<<20, "big-skin: target Login request size in bytes")
	fs.IntVar(&c.segmentSize, "segment-size", defaultSegmentSize, "nethernet: payload bytes per sent segment")
	fs.DurationVar(&c.firstSendDelay, "first-send-delay", 0, "time to wait after connecting before the first packet")
	fs.StringVar(&c.script, "script", "", "script: the file of actions to run (api.go)")
	if err := fs.Parse(args); err != nil {
		return c, err
	}
	if c.signalingAddr == "" {
		c.signalingAddr = c.addr
	}
	switch {
	case fs.NArg() != 0:
		return c, fmt.Errorf("unexpected arguments %v", fs.Args())
	case c.addr == "" || c.out == "":
		return c, errors.New("-addr and -out are required")
	case !knownScenario(c.scenario):
		return c, fmt.Errorf("unknown -scenario %q (want one of %v)", c.scenario, scenarioList())
	case c.transport != "raknet" && c.transport != "nethernet" && c.transport != "auto":
		return c, fmt.Errorf("unknown -transport %q (want raknet, nethernet or auto)", c.transport)
	case c.scenario == "bind-mismatch" && c.transport != "nethernet":
		return c, errors.New("-scenario bind-mismatch needs -transport nethernet")
	case (c.scenario == "idle" || c.scenario == "prelogin-limit") && c.transport != "raknet":
		return c, fmt.Errorf("-scenario %v checks RakNet only", c.scenario)
	case c.scenario == "online" && (c.tokenKey == "" || c.kid == "" || c.iss == ""):
		return c, errors.New("-scenario online needs -token-key, -kid and -iss")
	case (c.scenario == "script") != (c.script != ""):
		return c, errors.New("-script goes with -scenario script, and only with it")
	case c.loginSize < 0 || c.loginSize > 64<<20:
		return c, errors.New("-login-size must be between 0 and 64 MiB")
	case c.segmentSize < 1 || c.segmentSize > defaultSegmentSize:
		return c, fmt.Errorf("-segment-size must be between 1 and %v", defaultSegmentSize)
	case c.firstSendDelay < 0 || c.firstSendDelay > time.Minute:
		return c, errors.New("-first-send-delay must be between 0 and 1m")
	}
	return c, nil
}

// connectionScenarios are the scenarios that are not game scenarios (gameScenarios).
var connectionScenarios = []string{
	"spawn", "big-skin", "online", "kicked", "rejected", "stall", "blocks", "bind-mismatch", "idle", "prelogin-limit",
}

// scenarioList names every scenario the bot runs: connectionScenarios, then the game scenarios in name order.
func scenarioList() string {
	names := append(slices.Clone(connectionScenarios), slices.Sorted(maps.Keys(gameScenarios))...)
	return strings.Join(names, ", ")
}

// knownScenario reports whether name is a scenario the bot runs.
func knownScenario(name string) bool {
	return slices.Contains(connectionScenarios, name) || isGame(name)
}

// output writes JSON lines; a game scenario's script writes from its own goroutine.
type output struct {
	mu  sync.Mutex
	w   *bufio.Writer
	err error
}

func (o *output) line(v map[string]any) {
	o.mu.Lock()
	defer o.mu.Unlock()
	if o.err != nil {
		return
	}
	b, err := json.Marshal(v)
	if err != nil {
		o.err = err
		return
	}
	b = append(b, '\n')
	_, o.err = o.w.Write(b)
}

func (o *output) flush() {
	o.mu.Lock()
	defer o.mu.Unlock()
	if o.err == nil {
		o.err = o.w.Flush()
	}
}

func run(args []string) (bool, error) {
	c, err := parseFlags(args)
	if err != nil {
		return false, err
	}
	f, err := os.Create(c.out)
	if err != nil {
		return false, err
	}
	out := &output{w: bufio.NewWriterSize(f, 1<<20)}
	ctx, cancel := context.WithTimeout(context.Background(), c.timeout)
	defer cancel()

	var res *result
	if c.scenario == "idle" || c.scenario == "prelogin-limit" {
		res = playLimit(ctx, c)
	} else {
		items, closeItems, err := sideOutput(itemsPath(c.out))
		if err != nil {
			return false, err
		}
		world, closeWorld, err := sideOutput(worldPath(c.out))
		if err != nil {
			_ = closeItems()
			return false, err
		}
		res = play(ctx, c, out, items, world)
		if err := errors.Join(closeItems(), closeWorld()); err != nil {
			return false, err
		}
	}
	out.line(res.json(c))
	out.flush()
	if err := f.Close(); err != nil && out.err == nil {
		out.err = err
	}
	if out.err != nil {
		return false, out.err
	}
	if !res.pass {
		fmt.Fprintln(os.Stderr, "bot: scenario", c.scenario, "failed:", res.reason)
	}
	return res.pass, nil
}

// sideOutput creates a file of full packets next to the capture; close flushes and closes it and returns the
// first write error.
func sideOutput(path string) (*output, func() error, error) {
	f, err := os.Create(path)
	if err != nil {
		return nil, nil, err
	}
	o := &output{w: bufio.NewWriterSize(f, 1<<20)}
	return o, func() error {
		o.flush()
		if err := f.Close(); err != nil && o.err == nil {
			o.err = err
		}
		return o.err
	}, nil
}

// itemsPath is the file of the full item packets that belongs to the capture out.
func itemsPath(out string) string {
	return strings.TrimSuffix(out, ".jsonl") + ".items.jsonl"
}

// worldPath is the file of the full world packets that belongs to the capture out.
func worldPath(out string) string {
	return strings.TrimSuffix(out, ".jsonl") + ".world.jsonl"
}

func main() {
	if len(os.Args) > 1 && os.Args[1] == "worlddata" {
		same, err := worlddataCommand(os.Args[2:], os.Stdout)
		if err != nil {
			fmt.Fprintln(os.Stderr, "bot:", err)
			os.Exit(2)
		}
		if !same {
			os.Exit(1)
		}
		return
	}
	if len(os.Args) > 1 && os.Args[1] == "issuer" {
		if err := issuerCommand(os.Args[2:]); err != nil {
			fmt.Fprintln(os.Stderr, "bot:", err)
			os.Exit(2)
		}
		return
	}
	pass, err := run(os.Args[1:])
	if err != nil {
		fmt.Fprintln(os.Stderr, "bot:", err)
		os.Exit(2)
	}
	if !pass {
		os.Exit(1)
	}
}
