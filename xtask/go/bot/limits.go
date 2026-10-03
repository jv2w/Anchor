package main

import (
	"context"
	"fmt"
	"time"

	"github.com/sandertv/go-raknet"
)

// preLoginPerIP is Anchor's limit of connections per IP that have not logged in (PLAN.md 10.2).
const preLoginPerIP = 4

// playLimit runs the scenarios that check server limits instead of a spawn:
//
//	idle            connect over RakNet, send nothing; pass if the server closes the connection
//	                within its login deadline (10 s) plus a margin
//	prelogin-limit  open preLoginPerIP+1 RakNet connections without logging in; pass if the
//	                server accepts exactly preLoginPerIP of them
func playLimit(ctx context.Context, c config) *result {
	res := &result{loginSuccessIndex: -1, packsInfoIndex: -1}
	switch c.scenario {
	case "idle":
		conn, err := raknet.Dialer{}.DialContext(ctx, c.addr)
		if err != nil {
			res.reason = "dial: " + err.Error()
			return res
		}
		defer conn.Close()
		start := time.Now()
		for {
			if _, err := conn.ReadPacket(); err != nil {
				break
			}
		}
		took := time.Since(start)
		res.pass = took >= 9*time.Second && took <= 20*time.Second && ctx.Err() == nil
		res.reason = fmt.Sprintf("the server closed an idle connection after %v", took.Round(time.Millisecond))
	case "prelogin-limit":
		var accepted int
		for i := 0; i <= preLoginPerIP; i++ {
			dctx, cancel := context.WithTimeout(ctx, 5*time.Second)
			conn, err := raknet.Dialer{}.DialContext(dctx, c.addr)
			cancel()
			if err != nil {
				continue
			}
			defer conn.Close()
			accepted++
		}
		res.pass = accepted == preLoginPerIP
		res.reason = fmt.Sprintf("%v of %v connections accepted before login", accepted, preLoginPerIP+1)
	}
	return res
}
