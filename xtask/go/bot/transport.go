package main

import (
	"context"
	"crypto/ecdsa"
	"fmt"
	"time"

	"github.com/sandertv/go-raknet"
)

// nethernetBudget bounds the NetherNet attempt of -transport auto before it falls back to RakNet.
const nethernetBudget = 10 * time.Second

// packetConn carries whole batches: a go-raknet Conn (0xfe header, Minecraft encryption) or a
// go-nethernet Conn (no header, no encryption). gophertunnel's Encoder and Decoder find the difference
// through the optional BatchHeader and DisableEncryption methods.
type packetConn interface {
	ReadPacket() ([]byte, error)
	Write(b []byte) (int, error)
	Close() error
}

// connect opens the connection over -transport and records the transport facts in res: NetherNet
// first unless it is raknet, then RakNet unless it is nethernet (PLAN.md 2, the client's order). The
// token is the online scenario's multiplayer token, which the NetherNet offer identity carries too.
func connect(ctx context.Context, c config, out *output, key *ecdsa.PrivateKey, token string, res *result) (packetConn, error) {
	if c.transport != "raknet" {
		// pion stops the ICE transport when the dial context ends, so the context of a dialled
		// connection ends with the connection; only an unfinished dial is cut at the budget.
		nctx, cancel := context.WithCancel(ctx)
		var budget *time.Timer
		if c.transport == "auto" {
			budget = time.AfterFunc(nethernetBudget, cancel)
		}
		conn, err := dialNetherNet(nctx, c, key, token, out, res)
		if budget != nil {
			budget.Stop()
		}
		if err == nil {
			context.AfterFunc(conn.Context(), cancel)
			res.transport = "nethernet"
			segments, err := hookSegments(conn, c.segmentSize)
			if err != nil {
				_ = conn.Close()
				return nil, fmt.Errorf("take over the data channels: %w", err)
			}
			res.segments = segments
			return segments, nil
		}
		cancel()
		res.nethernetError = err.Error()
		if c.transport == "nethernet" {
			return nil, err
		}
	}
	out.line(probeRakNet(ctx, c.addr))
	pong, err := raknet.Dialer{}.PingContext(ctx, c.addr)
	out.line(pongRecord(pong, err))
	conn, err := raknet.Dialer{}.DialContext(ctx, c.addr)
	if err != nil {
		return nil, err
	}
	res.transport = "raknet"
	res.localAddr = conn.LocalAddr().String()
	return conn, nil
}
