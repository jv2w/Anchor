// Command bot is a Bedrock client built from gophertunnel's packet, login and protocol packages and go-raknet.
// It joins a server over RakNet, records every packet it receives and checks one scenario. It mirrors the
// client side of gophertunnel's minecraft/conn.go but does not use minecraft.Dialer, so the login request can
// carry a token from a test issuer.
//
//	bot -addr HOST:PORT -scenario spawn|big-skin|online -out FILE.jsonl
//	    [-token-key key.pem -kid K -iss ISS] [-name N] [-timeout 30s] [-stay 2s] [-login-size BYTES]
//	bot issuer -dir DIR -kid K -iss ISS
//
// Scenarios:
//
//	spawn     offline login, reach PlayStatus(PlayerSpawn), send SetLocalPlayerAsInitialised, stay connected
//	big-skin  spawn with a valid ClientData whose Login request is about -login-size bytes
//	online    spawn with a login.Encode request whose token is minted with -token-key (RS256)
//	idle, prelogin-limit  server limit checks (limits.go)
//
// The output holds one JSON object per line, each with a "type": "raknet" (RakNet handshake probe), "pong",
// "batch" (raw batch facts), "packet" (a received packet), "sent" (a packet the bot sent), "blocks" (the block
// network ID check) and a final "result". The process exits 0 only if the scenario passed, 1 if it failed and
// 2 on bad arguments or I/O errors.
package main

import (
	"bufio"
	"context"
	"encoding/json"
	"errors"
	"flag"
	"fmt"
	"os"
	"time"
)

// config holds the command line flags.
type config struct {
	addr, scenario, out string
	tokenKey, kid, iss  string
	name                string
	timeout, stay       time.Duration
	loginSize           int
}

func parseFlags(args []string) (config, error) {
	var c config
	fs := flag.NewFlagSet("bot", flag.ContinueOnError)
	fs.StringVar(&c.addr, "addr", "", "server address HOST:PORT")
	fs.StringVar(&c.scenario, "scenario", "spawn", "spawn, big-skin, online, idle or prelogin-limit")
	fs.StringVar(&c.out, "out", "", "output JSON lines file")
	fs.StringVar(&c.tokenKey, "token-key", "", "online: PEM RSA private key of the test issuer")
	fs.StringVar(&c.kid, "kid", "", "online: key ID put in the token header")
	fs.StringVar(&c.iss, "iss", "", "online: token issuer")
	fs.StringVar(&c.name, "name", "AnchorBot", "player name")
	fs.DurationVar(&c.timeout, "timeout", 30*time.Second, "time limit for the whole scenario")
	fs.DurationVar(&c.stay, "stay", 2*time.Second, "time to stay connected after spawning")
	fs.IntVar(&c.loginSize, "login-size", 6<<20, "big-skin: target Login request size in bytes")
	if err := fs.Parse(args); err != nil {
		return c, err
	}
	switch {
	case fs.NArg() != 0:
		return c, fmt.Errorf("unexpected arguments %v", fs.Args())
	case c.addr == "" || c.out == "":
		return c, errors.New("-addr and -out are required")
	case !knownScenario(c.scenario):
		return c, fmt.Errorf("unknown -scenario %q (want spawn, big-skin, online, idle or prelogin-limit)", c.scenario)
	case c.scenario == "online" && (c.tokenKey == "" || c.kid == "" || c.iss == ""):
		return c, errors.New("-scenario online needs -token-key, -kid and -iss")
	case c.loginSize < 0 || c.loginSize > 64<<20:
		return c, errors.New("-login-size must be between 0 and 64 MiB")
	}
	return c, nil
}

// knownScenario reports whether name is a scenario the bot runs.
func knownScenario(name string) bool {
	switch name {
	case "spawn", "big-skin", "online", "idle", "prelogin-limit":
		return true
	}
	return false
}

// output writes JSON lines.
type output struct {
	w   *bufio.Writer
	err error
}

func (o *output) line(v map[string]any) {
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
		res = play(ctx, c, out)
	}
	out.line(res.json(c.scenario))
	if err := out.w.Flush(); err != nil && out.err == nil {
		out.err = err
	}
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

func main() {
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
